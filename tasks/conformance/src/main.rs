//! Conformance test runner for oxc_react_compiler.
//!
//! Walks upstream fixtures from third_party/react, runs the compiler on each,
//! and compares output against `.expect.md` golden files.

mod fixtures;
mod normalizations;
mod pragmas;
mod reporting;

use std::path::{Path, PathBuf};

use fixtures::{
    FixtureOutcome, FixtureResult, Status, collect_fixtures, find_fixture_dir, run_fixture_suite,
};
use reporting::{
    build_failure_report, extract_cache_size, generate_snapshot, has_memo_cache,
    is_transformed_output_mismatch, print_regression_vs, write_failure_json_report,
};

#[derive(Clone, Debug)]
pub(crate) struct JsRuntime {
    pub(crate) executable: PathBuf,
    pub(crate) run_as_node: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureSuiteOptions {
    pub(crate) fixture_timeout: std::time::Duration,
    pub(crate) run_skipped: bool,
    pub(crate) parallel: bool,
    pub(crate) verbose: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let filter = args
        .iter()
        .position(|a| a == "--filter")
        .and_then(|i| args.get(i + 1));
    let update = args.iter().any(|a| a == "--update");
    let diff = args.iter().any(|a| a == "--diff");
    let show = args.iter().any(|a| a == "--show");
    let list = args.iter().any(|a| a == "--list");
    let categorize = args.iter().any(|a| a == "--categorize");
    let near_miss = args.iter().any(|a| a == "--near-miss");
    let near_miss_threshold: usize = args
        .iter()
        .position(|a| a == "--near-miss")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let include_errors = args.iter().any(|a| a == "--include-errors");
    let run_skipped = args.iter().any(|a| a == "--run-skipped");
    let parallel = !args.iter().any(|a| a == "--no-parallel");
    let verbose = args.iter().any(|a| a == "--verbose");
    let failures_json_path = args
        .iter()
        .position(|a| a == "--failures-json")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let regression_vs_path = args
        .iter()
        .position(|a| a == "--regression-vs")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);

    let fixture_dir = find_fixture_dir();
    let fixtures = collect_fixtures(&fixture_dir, filter.map(String::as_str));

    println!("Found {} fixtures", fixtures.len());

    let mut parity_success: usize = 0;
    let mut parity_failure: usize = 0;
    let mut skipped: usize = 0;

    let fixture_timeout_secs = args
        .iter()
        .position(|a| a == "--fixture-timeout")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .or_else(|| {
            std::env::var("FIXTURE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(10);
    let fixture_timeout = std::time::Duration::from_secs(fixture_timeout_secs);

    let results_raw: Vec<FixtureResult> = run_fixture_suite(
        &fixtures,
        FixtureSuiteOptions {
            fixture_timeout,
            run_skipped,
            parallel,
            verbose,
        },
    );
    let results: Vec<FixtureResult> = results_raw
        .into_iter()
        .map(|mut result| {
            // If --include-errors is not set, skip error fixtures (backward compatible)
            if result.is_error_fixture && !include_errors {
                result.status = Status::Skip;
                result.message =
                    Some("Error fixture (use --include-errors to include)".to_string());
            }
            result
        })
        .collect();

    for result in &results {
        if diff && matches!(result.status, Status::Fail) {
            eprintln!("\n=== FAIL: {} ===", result.name);
            if let Some(ref actual) = result.actual_code
                && let Some(ref expected) = result.expected_code
            {
                // Show a simple diff
                let actual_lines: Vec<&str> = actual.lines().collect();
                let expected_lines: Vec<&str> = expected.lines().collect();
                let max = actual_lines.len().max(expected_lines.len());
                for i in 0..max {
                    let a = actual_lines.get(i).unwrap_or(&"");
                    let e = expected_lines.get(i).unwrap_or(&"");
                    if a != e {
                        eprintln!("  line {}: ", i + 1);
                        eprintln!("    actual:   {}", a);
                        eprintln!("    expected: {}", e);
                    }
                }
            }
        }
        if show && matches!(result.status, Status::Fail) {
            eprintln!("\n=== SHOW: {} ===", result.name);
            if let Some(ref actual) = result.actual_code {
                eprintln!("--- ACTUAL ---");
                eprintln!("{}", actual);
            }
            if let Some(ref expected) = result.expected_code {
                eprintln!("--- EXPECTED ---");
                eprintln!("{}", expected);
            }
        }
        if list {
            let status_str = match result.status {
                Status::Pass => "PASS",
                Status::Fail => "FAIL",
                Status::Skip => "SKIP",
            };
            println!("{}: {}", status_str, result.name);
        }
        match result.status {
            Status::Pass => parity_success += 1,
            Status::Fail => parity_failure += 1,
            Status::Skip => skipped += 1,
        }
    }

    // Near-miss analysis: show failing fixtures with small diffs
    if near_miss {
        let mut near_misses: Vec<(usize, &str)> = Vec::new();
        for r in &results {
            if !matches!(r.status, Status::Fail) {
                continue;
            }
            if let (Some(actual), Some(expected)) = (&r.actual_code, &r.expected_code) {
                let actual_lines: Vec<&str> = actual.lines().collect();
                let expected_lines: Vec<&str> = expected.lines().collect();
                // Count lines that differ
                let max_len = actual_lines.len().max(expected_lines.len());
                let mut diff_lines = 0;
                for i in 0..max_len {
                    let a = actual_lines.get(i).unwrap_or(&"");
                    let e = expected_lines.get(i).unwrap_or(&"");
                    if a != e {
                        diff_lines += 1;
                    }
                }
                // Add extra lines if lengths differ
                if actual_lines.len() != expected_lines.len() {
                    let len_diff = (actual_lines.len() as isize - expected_lines.len() as isize)
                        .unsigned_abs();
                    if len_diff > diff_lines {
                        diff_lines = len_diff;
                    }
                }
                if diff_lines <= near_miss_threshold {
                    near_misses.push((diff_lines, &r.name));
                }
            }
        }
        near_misses.sort_by_key(|(d, _)| *d);
        println!("\n=== Near-miss failures (≤{near_miss_threshold} diff lines) ===");
        for (diff_count, name) in &near_misses {
            println!("{diff_count:3} | {name}");
        }
        println!("Total near-miss: {}", near_misses.len());
    }

    // Categorize failures
    if categorize {
        let mut false_memo = 0u32;
        let mut wrong_cache = 0u32;
        let mut same_cache_body_diff = 0u32;
        let mut no_output = 0u32;
        let mut timeout_failures = 0u32;
        let mut unexpected_skip = 0u32;
        let mut unexpected_error = 0u32;
        let mut harness_failures = 0u32;
        let mut state_mismatch = 0u32;
        for r in &results {
            if !matches!(r.status, Status::Fail) {
                continue;
            }
            match r.outcome {
                FixtureOutcome::Timeout => {
                    timeout_failures += 1;
                    continue;
                }
                FixtureOutcome::UnexpectedSkip => {
                    unexpected_skip += 1;
                    continue;
                }
                FixtureOutcome::UnexpectedError => {
                    unexpected_error += 1;
                    continue;
                }
                FixtureOutcome::HarnessFailure => {
                    harness_failures += 1;
                    continue;
                }
                FixtureOutcome::Mismatch => {
                    if !is_transformed_output_mismatch(r) {
                        state_mismatch += 1;
                        continue;
                    }
                }
                _ => {
                    state_mismatch += 1;
                    continue;
                }
            }
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            let actual_has_c = has_memo_cache(actual);
            let expected_has_c = has_memo_cache(expected);
            if actual.trim().is_empty() {
                no_output += 1;
            } else if actual_has_c && !expected_has_c {
                false_memo += 1;
            } else if actual_has_c && expected_has_c {
                // Extract cache sizes
                let actual_cache = extract_cache_size(actual);
                let expected_cache = extract_cache_size(expected);
                if actual_cache != expected_cache {
                    wrong_cache += 1;
                } else {
                    same_cache_body_diff += 1;
                }
            } else {
                // Other: expected has _c but we don't, or neither has _c
                same_cache_body_diff += 1;
            }
        }
        let mut false_memo_names: Vec<&str> = Vec::new();
        for r in &results {
            if !is_transformed_output_mismatch(r) {
                continue;
            }
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            if !actual.trim().is_empty() && has_memo_cache(actual) && !has_memo_cache(expected) {
                false_memo_names.push(&r.name);
            }
        }
        // Sub-categorize: expected_has_c but we don't, vs neither has _c
        let mut missing_memo = 0u32;
        let mut neither_memo = 0u32;
        let mut missing_memo_names = Vec::new();
        let mut neither_memo_names = Vec::new();
        for r in &results {
            if !is_transformed_output_mismatch(r) {
                continue;
            }
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            let actual_has_c = has_memo_cache(actual);
            let expected_has_c = has_memo_cache(expected);
            if !actual.trim().is_empty() && !actual_has_c && expected_has_c {
                missing_memo += 1;
                missing_memo_names.push(r.name.clone());
            } else if !actual.trim().is_empty() && !actual_has_c && !expected_has_c {
                neither_memo += 1;
                neither_memo_names.push(r.name.clone());
            }
        }
        println!("\n=== Failure Categories ===");
        println!("false_memo (we add _c, expected doesn't): {false_memo}");
        println!("wrong_cache (different _c(N) size): {wrong_cache}");
        println!("same_cache_body_diff (same _c(N), diff body): {same_cache_body_diff}");
        println!("  missing_memo (expected has _c, we don't): {missing_memo}");
        println!("  neither_memo (neither has _c): {neither_memo}");
        println!("no_output (empty compiler output): {no_output}");
        println!("timeout: {timeout_failures}");
        println!("unexpected_skip: {unexpected_skip}");
        println!("unexpected_error: {unexpected_error}");
        println!("harness_failure: {harness_failures}");
        println!("state_mismatch: {state_mismatch}");
        if !false_memo_names.is_empty() {
            println!("\n=== False Memo Fixtures ({}) ===", false_memo_names.len());
            for name in &false_memo_names {
                println!("  {name}");
            }
        }
        if !neither_memo_names.is_empty() {
            println!(
                "\n=== Neither Memo Fixtures ({}) ===",
                neither_memo_names.len()
            );
            for name in &neither_memo_names {
                println!("  {name}");
            }
        }
        if !missing_memo_names.is_empty() {
            println!(
                "\n=== Missing Memo Fixtures ({}) ===",
                missing_memo_names.len()
            );
            for name in &missing_memo_names {
                println!("  {name}");
            }
        }
        // Wrong cache size distribution
        {
            let mut size_pairs: std::collections::HashMap<(u32, u32), u32> =
                std::collections::HashMap::new();
            for r in &results {
                if !is_transformed_output_mismatch(r) {
                    continue;
                }
                let actual = r.actual_code.as_deref().unwrap_or("");
                let expected = r.expected_code.as_deref().unwrap_or("");
                if has_memo_cache(actual) && has_memo_cache(expected) {
                    let ac = extract_cache_size(actual).unwrap_or(0);
                    let ec = extract_cache_size(expected).unwrap_or(0);
                    if ac != ec {
                        *size_pairs.entry((ac, ec)).or_insert(0) += 1;
                    }
                }
            }
            let mut pairs: Vec<_> = size_pairs.into_iter().collect();
            pairs.sort_by_key(|b| std::cmp::Reverse(b.1));
            println!("\n=== Wrong Cache Size Distribution ===");
            for ((actual, expected), count) in &pairs {
                println!("  _c({actual}) vs expected _c({expected}): {count}");
            }
            // Print fixture names for _c(2)->_c(1) and _c(1)->_c(2)
            let mut c2_to_c1: Vec<String> = Vec::new();
            let mut c1_to_c2: Vec<String> = Vec::new();
            for r in &results {
                if !is_transformed_output_mismatch(r) {
                    continue;
                }
                let actual = r.actual_code.as_deref().unwrap_or("");
                let expected = r.expected_code.as_deref().unwrap_or("");
                if has_memo_cache(actual) && has_memo_cache(expected) {
                    let ac = extract_cache_size(actual).unwrap_or(0);
                    let ec = extract_cache_size(expected).unwrap_or(0);
                    if ac == 2 && ec == 1 {
                        c2_to_c1.push(r.name.clone());
                    } else if ac == 1 && ec == 2 {
                        c1_to_c2.push(r.name.clone());
                    }
                }
            }
            if !c2_to_c1.is_empty() {
                println!(
                    "\n=== _c(2) -> expected _c(1) [{} fixtures] ===",
                    c2_to_c1.len()
                );
                for name in &c2_to_c1 {
                    println!("  {name}");
                }
            }
            if !c1_to_c2.is_empty() {
                println!(
                    "\n=== _c(1) -> expected _c(2) [{} fixtures] ===",
                    c1_to_c2.len()
                );
                for name in &c1_to_c2 {
                    println!("  {name}");
                }
            }
        }
        // Count _c(1) same-cache (both sides have _c(1))
        let mut c1_body_diff = 0u32;
        let mut c1_body_diff_names = Vec::new();
        let mut same_cache_names = Vec::new();
        for r in &results {
            if !is_transformed_output_mismatch(r) {
                continue;
            }
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            if has_memo_cache(actual) && has_memo_cache(expected) {
                let ac = extract_cache_size(actual);
                let ec = extract_cache_size(expected);
                if ac == ec {
                    same_cache_names.push(r.name.clone());
                    if ac == Some(1) {
                        c1_body_diff += 1;
                        c1_body_diff_names.push(r.name.clone());
                    }
                }
            }
        }
        println!("  _c(1) body_diff: {c1_body_diff}");
        if !c1_body_diff_names.is_empty() {
            println!("\n=== _c(1) Body Diff Fixtures ===");
            for name in &c1_body_diff_names {
                println!("  {name}");
            }
        }
        if !false_memo_names.is_empty() {
            println!("\n=== False Memo Fixtures ===");
            for name in &false_memo_names {
                println!("  {name}");
            }
        }
        if !same_cache_names.is_empty() {
            println!(
                "\n=== Same Cache Body Diff Fixtures ({}) ===",
                same_cache_names.len()
            );
            for name in &same_cache_names {
                println!("  {name}");
            }
        }
        // Also print "neither_has_c" (expected doesn't have _c, we also don't)
        let mut neither_names: Vec<String> = Vec::new();
        for r in &results {
            if !is_transformed_output_mismatch(r) {
                continue;
            }
            let actual = r.actual_code.as_deref().unwrap_or("");
            let expected = r.expected_code.as_deref().unwrap_or("");
            if !has_memo_cache(actual) && !has_memo_cache(expected) && !actual.trim().is_empty() {
                neither_names.push(r.name.clone());
            }
        }
        if !neither_names.is_empty() {
            println!(
                "\n=== Neither Has _c Fixtures ({}) ===",
                neither_names.len()
            );
            for name in &neither_names {
                println!("  {name}");
            }
        }
    }

    let total = fixtures.len();
    let parity_rate = if total > 0 {
        (parity_success as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    // Print all results for diffing
    if std::env::var("PRINT_ALL").is_ok() {
        for r in &results {
            match r.status {
                Status::Pass => println!("PASS: {}", r.name),
                Status::Fail => println!("FAIL: {}", r.name),
                Status::Skip => {}
            }
        }
    }

    // Error fixture sub-counts
    let error_passed: usize = results
        .iter()
        .filter(|r| r.is_error_fixture && matches!(r.status, Status::Pass))
        .count();
    let error_failed: usize = results
        .iter()
        .filter(|r| r.is_error_fixture && matches!(r.status, Status::Fail))
        .count();
    let error_skipped: usize = results
        .iter()
        .filter(|r| r.is_error_fixture && matches!(r.status, Status::Skip))
        .count();
    let error_total = error_passed + error_failed + error_skipped;

    println!(
        "\nResults: {parity_success} parity_success, {parity_failure} parity_failure, {skipped} skipped ({total} total)"
    );
    println!("Parity rate: {parity_rate:.1}%");
    if include_errors && error_total > 0 {
        println!(
            "Error fixtures: {error_passed} passed, {error_failed} failed ({error_total} total)"
        );
    } else if error_total > 0 {
        println!("Error fixtures: {error_total} skipped (use --include-errors to include)");
    }

    let failure_report = build_failure_report(
        &results,
        include_errors,
        parity_success,
        parity_failure,
        skipped,
    );
    if let Some(path) = failures_json_path.as_ref() {
        if let Err(err) = write_failure_json_report(path, &failure_report) {
            eprintln!("[ERROR] {err}");
            std::process::exit(2);
        }
        println!("Wrote failures json to {}", path.display());
    }
    if let Some(path) = regression_vs_path.as_ref()
        && let Err(err) = print_regression_vs(path, &failure_report)
    {
        eprintln!("[ERROR] {err}");
        std::process::exit(2);
    }

    if update {
        let snapshot = generate_snapshot(&results, parity_rate);
        let snapshot_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("snapshots/react_compiler.snap.md");
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(&snapshot_path, snapshot).unwrap();
        println!("Updated snapshot at {}", snapshot_path.display());
    }

    if parity_failure > 0 {
        std::process::exit(1);
    }
}
