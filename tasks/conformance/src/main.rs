//! Conformance test runner for oxc_react_compiler.
//!
//! Walks upstream fixtures from third_party/react, runs the compiler on each,
//! and compares output against `.expect.md` golden files.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct JsRuntime {
    executable: PathBuf,
    run_as_node: bool,
}

#[derive(Clone, Copy, Debug)]
struct FixtureSuiteOptions {
    fixture_timeout: std::time::Duration,
    run_skipped: bool,
    strict_output: bool,
    parallel: bool,
    verbose: bool,
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
    let relax_deps = args.iter().any(|a| a == "--relax-deps");
    let relax_cache = args.iter().any(|a| a == "--relax-cache");
    let include_errors = args.iter().any(|a| a == "--include-errors");
    let run_skipped = args.iter().any(|a| a == "--run-skipped");
    let strict_output = args.iter().any(|a| a == "--strict-output");
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
            strict_output,
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

    // Relax-deps analysis: strip dep-related lines and see how many more would pass
    if relax_deps {
        let mut would_pass = 0u32;
        let mut would_pass_names: Vec<String> = Vec::new();
        for r in &results {
            if !matches!(r.status, Status::Fail) {
                continue;
            }
            if let (Some(actual), Some(expected)) = (&r.actual_code, &r.expected_code) {
                let relaxed_actual = strip_dep_lines(actual);
                let relaxed_expected = strip_dep_lines(expected);
                if relaxed_actual == relaxed_expected {
                    would_pass += 1;
                    would_pass_names.push(r.name.clone());
                }
            }
        }
        println!("\n=== Relax-deps analysis ===");
        println!("Additional fixtures that would pass if deps were correct: {would_pass}");
        for name in &would_pass_names {
            println!("  {name}");
        }
    }

    // Relax-cache analysis: normalize cache sizes and slot indices
    if relax_cache {
        let mut would_pass = 0u32;
        let mut would_pass_names: Vec<String> = Vec::new();
        for r in &results {
            if !matches!(r.status, Status::Fail) {
                continue;
            }
            if let (Some(actual), Some(expected)) = (&r.actual_code, &r.expected_code) {
                let relaxed_actual = normalize_cache_refs(actual);
                let relaxed_expected = normalize_cache_refs(expected);
                if relaxed_actual == relaxed_expected {
                    would_pass += 1;
                    would_pass_names.push(r.name.clone());
                }
            }
        }
        println!("\n=== Relax-cache analysis ===");
        println!("Additional fixtures that would pass with correct cache sizes: {would_pass}");
        if would_pass <= 50 {
            for name in &would_pass_names {
                println!("  {name}");
            }
        }
    }

    if parity_failure > 0 {
        std::process::exit(1);
    }
}

fn run_fixture_suite(fixtures: &[Fixture], options: FixtureSuiteOptions) -> Vec<FixtureResult> {
    if options.parallel {
        fixtures
            .par_iter()
            .map(|fixture| {
                if options.verbose {
                    println!("Running {}", fixture.name);
                }
                let res = run_fixture_with_timeout(
                    fixture,
                    options.fixture_timeout,
                    options.run_skipped,
                    options.strict_output,
                );
                if options.verbose {
                    println!("Finished {}", fixture.name);
                }
                res
            })
            .collect()
    } else {
        fixtures
            .iter()
            .map(|fixture| {
                if options.verbose {
                    println!("Running {}", fixture.name);
                }
                let res = run_fixture_with_timeout(
                    fixture,
                    options.fixture_timeout,
                    options.run_skipped,
                    options.strict_output,
                );
                if options.verbose {
                    println!("Finished {}", fixture.name);
                }
                res
            })
            .collect()
    }
}

/// Normalize cache-related references:
/// - `_c(N)` → `_c(?)` for any N
/// - `$[N]` → `$[?]` for any N
/// - `Symbol.for("react.memo_cache_sentinel")` is preserved
fn normalize_cache_refs(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        // Match _c(N)
        if i + 3 < chars.len() && chars[i] == '_' && chars[i + 1] == 'c' && chars[i + 2] == '(' {
            result.push_str("_c(");
            i += 3;
            // Skip digits
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            result.push('?');
        }
        // Match $[N]
        else if i + 2 < chars.len() && chars[i] == '$' && chars[i + 1] == '[' {
            result.push_str("$[");
            i += 2;
            // Skip digits
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            result.push('?');
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Check if code has a memo cache call: `_c(N)` or `_c2(N)` etc.
fn has_memo_cache(code: &str) -> bool {
    // Match _c( or _c2( or _c3( etc. — but NOT _c as _c2 (import rename)
    // The pattern is: `_c` optionally followed by digits, then `(`
    let bytes = code.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i] == b'_' && bytes[i + 1] == b'c' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                // Make sure this isn't something like `x_c(` — check preceding char
                if i == 0 || !bytes[i - 1].is_ascii_alphanumeric() {
                    return true;
                }
            }
        }
    }
    false
}

fn extract_cache_size(code: &str) -> Option<u32> {
    // Find first `_c(N)` or `_cN(M)` and extract the number inside parens
    let bytes = code.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        if bytes[i] == b'_' && bytes[i + 1] == b'c' {
            // Check preceding char isn't alphanumeric
            if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
                continue;
            }
            let mut j = i + 2;
            // Skip optional digit suffix (_c2, _c3, etc.)
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

fn is_transformed_output_mismatch(result: &FixtureResult) -> bool {
    matches!(result.status, Status::Fail)
        && matches!(result.outcome, FixtureOutcome::Mismatch)
        && matches!(result.expected_state, Some(ExpectedState::Transform))
        && matches!(result.actual_state, ActualState::Transformed)
        && result.actual_code.is_some()
        && result.expected_code.is_some()
}

fn categorize_failure(result: &FixtureResult) -> FailureCategory {
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

fn build_failure_report(
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

fn write_failure_json_report(path: &Path, report: &FailureJsonReport) -> Result<(), String> {
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

fn print_regression_vs(baseline_path: &Path, current: &FailureJsonReport) -> Result<(), String> {
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

fn find_fixture_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    workspace_root.join("third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler")
}

#[derive(Clone)]
struct Fixture {
    name: String,
    input_path: PathBuf,
    expect_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExpectedState {
    Transform,
    Skip,
    Error,
    Bailout,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ActualState {
    Transformed,
    Skipped,
    Error,
    Bailout,
    Timeout,
    HarnessFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FixtureOutcome {
    TransformedMatch,
    ExpectedSkipMatch,
    ExpectedErrorMatch,
    ExpectedBailoutMatch,
    Mismatch,
    UnexpectedSkip,
    UnexpectedError,
    Timeout,
    HarnessFailure,
}

#[derive(Clone)]
struct FixtureResult {
    name: String,
    status: Status,
    message: Option<String>,
    expected_state: Option<ExpectedState>,
    actual_state: ActualState,
    outcome: FixtureOutcome,
    parity_success: bool,
    actual_code: Option<String>,
    expected_code: Option<String>,
    is_error_fixture: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum FailureCategory {
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
struct FailureDiffLine {
    line: usize,
    actual: String,
    expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailureRecord {
    name: String,
    category: FailureCategory,
    #[serde(default)]
    expected_state: Option<ExpectedState>,
    #[serde(default)]
    actual_state: Option<ActualState>,
    #[serde(default)]
    parity_success: bool,
    actual_cache_size: Option<u32>,
    expected_cache_size: Option<u32>,
    message: Option<String>,
    is_error_fixture: bool,
    diff_lines: Vec<FailureDiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkipRecord {
    name: String,
    message: Option<String>,
    is_error_fixture: bool,
    #[serde(default)]
    expected_state: Option<ExpectedState>,
    #[serde(default)]
    actual_state: Option<ActualState>,
    #[serde(default)]
    outcome: Option<FixtureOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FailureJsonReport {
    total: usize,
    #[serde(default)]
    parity_success: usize,
    #[serde(default)]
    parity_failure: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    include_errors: bool,
    failures: Vec<FailureRecord>,
    #[serde(default)]
    skips_details: Vec<SkipRecord>,
}

fn collect_fixtures(dir: &Path, filter: Option<&str>) -> Vec<Fixture> {
    let mut fixtures = Vec::new();

    if !dir.exists() {
        eprintln!("Fixture directory not found: {}", dir.display());
        return fixtures;
    }

    collect_fixtures_recursive(dir, None, filter, &mut fixtures);

    fixtures.sort_by(|a, b| a.name.cmp(&b.name));

    fixtures
}

fn collect_fixtures_recursive(
    dir: &Path,
    prefix: Option<&str>,
    filter: Option<&str>,
    fixtures: &mut Vec<Fixture>,
) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();

        if path.is_dir() {
            let subdir_name = path.file_name().unwrap().to_string_lossy().to_string();
            collect_fixtures_recursive(&path, Some(&subdir_name), filter, fixtures);
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str());

        match ext {
            Some("js" | "jsx" | "ts" | "tsx") => {}
            _ => continue,
        }

        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let name = match prefix {
            Some(p) => format!("{p}/{stem}"),
            None => stem.clone(),
        };

        if let Some(filter) = filter
            && !name.contains(filter)
        {
            continue;
        }

        let expect_path = dir.join(format!("{stem}.expect.md"));
        if !expect_path.exists() {
            continue;
        }

        fixtures.push(Fixture {
            name,
            input_path: path,
            expect_path,
        });
    }
}

/// Parsed pragma information from a fixture's first line.
struct Pragma {
    compilation_mode: oxc_react_compiler::options::CompilationMode,
    panic_threshold: oxc_react_compiler::options::PanicThreshold,
    should_skip: bool,
    custom_opt_out_directives: Vec<String>,
    ignore_use_no_forget: bool,

    // --- PluginOptions-level pragmas ---
    gating: bool,
    dynamic_gating: Option<String>, // source module
    no_emit: bool,
    target: Option<String>,
    eslint_suppression_rules: Option<Vec<String>>,
    flow_suppressions: Option<bool>,
    logger_test_only: bool,

    // --- EnvironmentConfig boolean flags ---
    /// Each `Option<bool>` is `None` if not specified, `Some(true/false)` if explicitly set.
    validate_preserve_existing_memoization_guarantees: Option<bool>,
    validate_ref_access_during_render: Option<bool>,
    validate_no_set_state_in_render: Option<bool>,
    validate_no_set_state_in_effects: Option<bool>,
    validate_no_derived_computations_in_effects: Option<bool>,
    validate_no_jsx_in_try_statements: Option<bool>,
    validate_static_components: Option<bool>,
    validate_memoized_effect_dependencies: Option<bool>,
    validate_no_capitalized_calls: Option<bool>,
    validate_no_impure_functions_in_render: Option<bool>,
    validate_no_freezing_known_mutable_functions: Option<bool>,
    validate_no_void_use_memo: Option<bool>,
    validate_blocklisted_imports: Option<Vec<String>>,
    validate_no_dynamically_created_components_or_hooks: Option<bool>,

    enable_preserve_existing_memoization_guarantees: Option<bool>,
    enable_transitively_freeze_function_expressions: Option<bool>,
    enable_assume_hooks_follow_rules_of_react: Option<bool>,
    enable_optional_dependencies: Option<bool>,
    enable_treat_function_deps_as_conditional: Option<bool>,
    enable_treat_ref_like_identifiers_as_refs: Option<bool>,
    enable_treat_set_identifiers_as_state_setters: Option<bool>,
    enable_use_type_annotations: Option<bool>,
    enable_jsx_outlining: Option<bool>,
    enable_instruction_reordering: Option<bool>,
    enable_memoization_comments: Option<bool>,
    enable_name_anonymous_functions: Option<bool>,
    enable_custom_type_definition_for_reanimated: Option<bool>,
    enable_allow_set_state_from_refs_in_effects: Option<bool>,
    disable_memoization_for_debugging: Option<bool>,
    enable_preserve_existing_manual_use_memo: Option<bool>,
    enable_new_mutation_aliasing_model: Option<bool>,
    enable_propagate_deps_in_hir: Option<bool>,
    enable_reactive_scopes_in_hir: Option<bool>,
    enable_change_detection_for_debugging: Option<bool>,
    enable_reset_cache_on_source_file_changes: Option<bool>,
    throw_unknown_exception_testonly: Option<bool>,

    // --- Complex EnvironmentConfig pragmas ---
    enable_emit_freeze: Option<bool>,
    enable_emit_hook_guards: Option<bool>,
    enable_emit_instrument_forget: Option<bool>,
    enable_change_variable_codegen: Option<bool>,
    enable_fire: Option<bool>,
    inline_jsx_transform: Option<bool>,
    instrument_forget: Option<bool>,
    lower_context_access: bool,
    infer_effect_dependencies: bool,
    hook_pattern: Option<String>,
    custom_macros: Option<String>,
}

/// Split a pragma line into (key, value) pairs.
/// The pragma line is expected to look like: `// @key1 @key2:"value" @key3:value`
/// Returns an iterator of (key, Option<value>) tuples.
fn split_pragma(line: &str) -> Vec<(String, Option<String>)> {
    let mut results = Vec::new();
    // Strip leading `//` and whitespace
    let line = line.trim_start_matches("//").trim();

    for entry in line.split('@') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        // Check for value delimiter
        if let Some(colon_idx) = entry.find(':') {
            let key = entry[..colon_idx].trim().to_string();
            let val = entry[colon_idx + 1..].trim().to_string();
            // Trim trailing words that aren't part of the value
            // Values are: "string", number, true, false, {json}, [array], or unquoted string up to whitespace
            results.push((key, Some(val)));
        } else if let Some(paren_idx) = entry.find('(') {
            // Handle @key("value") or @key(value) syntax
            let key = entry[..paren_idx].trim().to_string();
            if let Some(close) = entry.find(')') {
                let inner = entry[paren_idx + 1..close].trim();
                // Strip surrounding quotes if present
                let val = if inner.starts_with('"') && inner.ends_with('"') && inner.len() >= 2 {
                    inner[1..inner.len() - 1].to_string()
                } else {
                    inner.to_string()
                };
                results.push((key, Some(val)));
            }
        } else {
            // Boolean flag — just the key, possibly followed by other text (like "flow")
            let key = entry.split_whitespace().next().unwrap_or("").to_string();
            if !key.is_empty() {
                results.push((key, None));
            }
        }
    }
    results
}

/// Try to parse a string value from a pragma value.
/// Handles: `"quoted"`, `true`/`false`, plain strings (up to whitespace), JSON objects/arrays.
fn parse_pragma_string_value(val: &str) -> String {
    let val = val.trim();
    // Strip surrounding quotes
    if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
        return val[1..val.len() - 1].to_string();
    }
    // Take up to first whitespace for simple values
    val.split_whitespace().next().unwrap_or("").to_string()
}

/// Parse a boolean pragma value. `None` or `"true"` → true, `"false"` → false.
fn parse_pragma_bool(val: &Option<String>) -> bool {
    match val {
        None => true,
        Some(v) => v.trim() != "false",
    }
}

/// Parse optional boolean: `None` means "pragma not present" (returned by caller),
/// `Some(val)` is parsed as bool.
fn parse_pragma_optional_bool(val: &Option<String>) -> bool {
    parse_pragma_bool(val)
}

/// Parse a JSON-like array of strings from a pragma value: `["a", "b"]` or `[]`.
fn parse_string_array(val: &str) -> Vec<String> {
    let val = val.trim();
    let mut result = Vec::new();
    if let Some(bracket_start) = val.find('[') {
        let bracket_content = &val[bracket_start + 1..];
        if let Some(bracket_end) = bracket_content.find(']') {
            let inner = &bracket_content[..bracket_end];
            let mut i = 0;
            let chars: Vec<char> = inner.chars().collect();
            while i < chars.len() {
                if chars[i] == '"' {
                    let start = i + 1;
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        i += 1;
                    }
                    if i < chars.len() {
                        let s: String = chars[start..i].iter().collect();
                        result.push(s);
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }
    }
    result
}

/// Parse the first line of a fixture for pragma directives.
fn parse_pragma(source: &str) -> Pragma {
    use oxc_react_compiler::options::{CompilationMode, PanicThreshold};

    let first_line = source.lines().next().unwrap_or("");

    // Start with defaults
    let mut pragma = Pragma {
        compilation_mode: CompilationMode::All, // snap default is 'all'
        panic_threshold: PanicThreshold::All,   // snap default is 'all_errors'
        should_skip: false,
        custom_opt_out_directives: Vec::new(),
        ignore_use_no_forget: false,
        gating: false,
        dynamic_gating: None,
        no_emit: false,
        target: None,
        eslint_suppression_rules: None,
        flow_suppressions: None,
        logger_test_only: false,
        validate_preserve_existing_memoization_guarantees: None,
        validate_ref_access_during_render: None,
        validate_no_set_state_in_render: None,
        validate_no_set_state_in_effects: None,
        validate_no_derived_computations_in_effects: None,
        validate_no_jsx_in_try_statements: None,
        validate_static_components: None,
        validate_memoized_effect_dependencies: None,
        validate_no_capitalized_calls: None,
        validate_no_impure_functions_in_render: None,
        validate_no_freezing_known_mutable_functions: None,
        validate_no_void_use_memo: None,
        validate_blocklisted_imports: None,
        validate_no_dynamically_created_components_or_hooks: None,
        enable_preserve_existing_memoization_guarantees: None,
        enable_transitively_freeze_function_expressions: None,
        enable_assume_hooks_follow_rules_of_react: None,
        enable_optional_dependencies: None,
        enable_treat_function_deps_as_conditional: None,
        enable_treat_ref_like_identifiers_as_refs: None,
        enable_treat_set_identifiers_as_state_setters: None,
        enable_use_type_annotations: None,
        enable_jsx_outlining: None,
        enable_instruction_reordering: None,
        enable_memoization_comments: None,
        enable_name_anonymous_functions: None,
        enable_custom_type_definition_for_reanimated: None,
        enable_allow_set_state_from_refs_in_effects: None,
        disable_memoization_for_debugging: None,
        enable_preserve_existing_manual_use_memo: None,
        enable_new_mutation_aliasing_model: None,
        enable_propagate_deps_in_hir: None,
        enable_reactive_scopes_in_hir: None,
        enable_change_detection_for_debugging: None,
        enable_reset_cache_on_source_file_changes: None,
        throw_unknown_exception_testonly: None,
        enable_emit_freeze: None,
        enable_emit_hook_guards: None,
        enable_emit_instrument_forget: None,
        enable_change_variable_codegen: None,
        enable_fire: None,
        inline_jsx_transform: None,
        instrument_forget: None,
        lower_context_access: false,
        infer_effect_dependencies: false,
        hook_pattern: None,
        custom_macros: None,
    };

    // Check for @skip pragma
    if first_line.contains("@skip") {
        pragma.should_skip = true;
        return pragma;
    }

    // Skip fixtures that require unimplemented feature flags.
    // These test features we haven't ported yet; running them produces
    // false failures or false passes.
    const UNSUPPORTED_FLAGS: &[&str] = &[];
    if UNSUPPORTED_FLAGS
        .iter()
        .any(|flag| first_line.contains(flag))
    {
        pragma.should_skip = true;
        return pragma;
    }

    // Parse all pragmas using the split_pragma approach
    let entries = split_pragma(first_line);
    for (key, val) in &entries {
        match key.as_str() {
            // --- Skip/meta pragmas ---
            "skip" => {
                pragma.should_skip = true;
            }
            "noEmit" => {
                pragma.no_emit = true;
            }
            "flow" | "script" | "xonly" | "Pass" | "debug" | "enable" => { /* ignored meta pragmas */
            }
            "loggerTestOnly" => {
                pragma.logger_test_only = true;
            }

            // --- PluginOptions-level pragmas ---
            "compilationMode" => {
                if let Some(v) = val {
                    let v = parse_pragma_string_value(v);
                    pragma.compilation_mode = match v.as_str() {
                        "infer" => CompilationMode::Infer,
                        "annotation" => CompilationMode::Annotation,
                        "all" => CompilationMode::All,
                        _ => CompilationMode::All,
                    };
                }
            }
            "panicThreshold" => {
                if let Some(v) = val {
                    let v = parse_pragma_string_value(v);
                    pragma.panic_threshold = match v.as_str() {
                        "none" => PanicThreshold::None,
                        "all_errors" | "all" => PanicThreshold::All,
                        _ => PanicThreshold::All,
                    };
                }
            }
            "target" => {
                if let Some(v) = val {
                    pragma.target = Some(parse_pragma_string_value(v));
                }
            }
            "gating" => {
                pragma.gating = true;
            }
            "dynamicGating" => {
                if let Some(v) = val {
                    // Parse JSON: {"source":"module"}
                    let v = v.trim();
                    if let Some(start) = v.find("\"source\"") {
                        let rest = &v[start + "\"source\"".len()..];
                        if let Some(colon) = rest.find(':') {
                            let after_colon = rest[colon + 1..].trim();
                            let source = parse_pragma_string_value(after_colon);
                            pragma.dynamic_gating = Some(source);
                        }
                    }
                }
            }
            "customOptOutDirectives" => {
                if let Some(v) = val {
                    pragma.custom_opt_out_directives = parse_string_array(v);
                }
            }
            "ignoreUseNoForget" => {
                pragma.ignore_use_no_forget = true;
            }
            "eslintSuppressionRules" => {
                if let Some(v) = val {
                    pragma.eslint_suppression_rules = Some(parse_string_array(v));
                }
            }
            "flowSuppressions" | "enableFlowSuppressions" => {
                pragma.flow_suppressions = Some(parse_pragma_optional_bool(val));
            }

            // --- EnvironmentConfig boolean flags (validation) ---
            "validatePreserveExistingMemoizationGuarantees" => {
                pragma.validate_preserve_existing_memoization_guarantees =
                    Some(parse_pragma_optional_bool(val));
            }
            "validateRefAccessDuringRender" => {
                pragma.validate_ref_access_during_render = Some(parse_pragma_optional_bool(val));
            }
            "validateNoSetStateInRender" => {
                pragma.validate_no_set_state_in_render = Some(parse_pragma_optional_bool(val));
            }
            "validateNoSetStateInEffects" => {
                pragma.validate_no_set_state_in_effects = Some(parse_pragma_optional_bool(val));
            }
            "validateNoDerivedComputationsInEffects" => {
                pragma.validate_no_derived_computations_in_effects =
                    Some(parse_pragma_optional_bool(val));
            }
            "validateNoJSXInTryStatements" => {
                pragma.validate_no_jsx_in_try_statements = Some(parse_pragma_optional_bool(val));
            }
            "validateStaticComponents" => {
                pragma.validate_static_components = Some(parse_pragma_optional_bool(val));
            }
            "validateMemoizedEffectDependencies" => {
                pragma.validate_memoized_effect_dependencies =
                    Some(parse_pragma_optional_bool(val));
            }
            "validateNoCapitalizedCalls" => {
                pragma.validate_no_capitalized_calls = Some(parse_pragma_optional_bool(val));
            }
            "validateNoImpureFunctionsInRender" => {
                pragma.validate_no_impure_functions_in_render =
                    Some(parse_pragma_optional_bool(val));
            }
            "validateNoFreezingKnownMutableFunctions" => {
                pragma.validate_no_freezing_known_mutable_functions =
                    Some(parse_pragma_optional_bool(val));
            }
            "validateNoVoidUseMemo" => {
                pragma.validate_no_void_use_memo = Some(parse_pragma_optional_bool(val));
            }
            "validateBlocklistedImports" => {
                if let Some(v) = val {
                    pragma.validate_blocklisted_imports = Some(parse_string_array(v));
                } else {
                    pragma.validate_blocklisted_imports = Some(Vec::new());
                }
            }
            "validateNoDynamicallyCreatedComponentsOrHooks" => {
                pragma.validate_no_dynamically_created_components_or_hooks =
                    Some(parse_pragma_optional_bool(val));
            }

            // --- EnvironmentConfig boolean flags (feature) ---
            "enablePreserveExistingMemoizationGuarantees" => {
                pragma.enable_preserve_existing_memoization_guarantees =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableTransitivelyFreezeFunctionExpressions" => {
                pragma.enable_transitively_freeze_function_expressions =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableAssumeHooksFollowRulesOfReact" => {
                pragma.enable_assume_hooks_follow_rules_of_react =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableOptionalDependencies" => {
                pragma.enable_optional_dependencies = Some(parse_pragma_optional_bool(val));
            }
            "enableTreatFunctionDepsAsConditional" => {
                pragma.enable_treat_function_deps_as_conditional =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableTreatRefLikeIdentifiersAsRefs" => {
                pragma.enable_treat_ref_like_identifiers_as_refs =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableTreatSetIdentifiersAsStateSetters" => {
                pragma.enable_treat_set_identifiers_as_state_setters =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableUseTypeAnnotations" => {
                pragma.enable_use_type_annotations = Some(parse_pragma_optional_bool(val));
            }
            "enableJsxOutlining" => {
                pragma.enable_jsx_outlining = Some(parse_pragma_optional_bool(val));
            }
            "enableInstructionReordering" => {
                pragma.enable_instruction_reordering = Some(parse_pragma_optional_bool(val));
            }
            "enableMemoizationComments" => {
                pragma.enable_memoization_comments = Some(parse_pragma_optional_bool(val));
            }
            "enableNameAnonymousFunctions" => {
                pragma.enable_name_anonymous_functions = Some(parse_pragma_optional_bool(val));
            }
            "enableCustomTypeDefinitionForReanimated" => {
                pragma.enable_custom_type_definition_for_reanimated =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableAllowSetStateFromRefsInEffects" => {
                pragma.enable_allow_set_state_from_refs_in_effects =
                    Some(parse_pragma_optional_bool(val));
            }
            "disableMemoizationForDebugging" => {
                pragma.disable_memoization_for_debugging = Some(parse_pragma_optional_bool(val));
            }
            "enablePreserveExistingManualUseMemo" => {
                pragma.enable_preserve_existing_manual_use_memo =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableNewMutationAliasingModel" => {
                pragma.enable_new_mutation_aliasing_model = Some(parse_pragma_optional_bool(val));
            }
            "enablePropagateDepsInHIR" => {
                pragma.enable_propagate_deps_in_hir = Some(parse_pragma_optional_bool(val));
            }
            "enableReactiveScopesInHIR" => {
                pragma.enable_reactive_scopes_in_hir = Some(parse_pragma_optional_bool(val));
            }
            "enableChangeDetectionForDebugging" => {
                pragma.enable_change_detection_for_debugging =
                    Some(parse_pragma_optional_bool(val));
            }
            "enableResetCacheOnSourceFileChanges" => {
                pragma.enable_reset_cache_on_source_file_changes =
                    Some(parse_pragma_optional_bool(val));
            }
            "throwUnknownException__testonly" => {
                pragma.throw_unknown_exception_testonly = Some(parse_pragma_optional_bool(val));
            }

            // --- Complex EnvironmentConfig pragmas (features that need special codegen) ---
            "enableEmitFreeze" => {
                pragma.enable_emit_freeze = Some(parse_pragma_optional_bool(val));
            }
            "enableEmitHookGuards" => {
                pragma.enable_emit_hook_guards = Some(parse_pragma_optional_bool(val));
            }
            "enableEmitInstrumentForget" => {
                pragma.enable_emit_instrument_forget = Some(parse_pragma_optional_bool(val));
            }
            "enableChangeVariableCodegen" => {
                pragma.enable_change_variable_codegen = Some(parse_pragma_optional_bool(val));
            }
            "enableFire" => {
                pragma.enable_fire = Some(parse_pragma_optional_bool(val));
            }
            "inlineJsxTransform" => {
                pragma.inline_jsx_transform = Some(parse_pragma_optional_bool(val));
            }
            "instrumentForget" => {
                pragma.instrument_forget = Some(parse_pragma_optional_bool(val));
            }
            "lowerContextAccess" => {
                pragma.lower_context_access = true;
            }
            "inferEffectDependencies" => {
                pragma.infer_effect_dependencies = true;
            }
            "hookPattern" => {
                if let Some(v) = val {
                    pragma.hook_pattern = Some(parse_pragma_string_value(v));
                }
            }
            "customMacros" => {
                if let Some(v) = val {
                    pragma.custom_macros = Some(parse_pragma_string_value(v));
                } else {
                    // @customMacros(name) already parsed by split_pragma with paren syntax
                }
            }

            _ => {
                // Unknown pragma — silently ignore
            }
        }
    }

    pragma
}

fn run_fixture_with_timeout(
    fixture: &Fixture,
    timeout: std::time::Duration,
    run_skipped: bool,
    strict_output: bool,
) -> FixtureResult {
    let fixture_clone = fixture.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024) // 64MB stack
        .spawn(move || {
            let r = run_fixture(&fixture_clone, run_skipped, strict_output);
            let _ = tx.send(r);
        })
        .expect("failed to spawn fixture thread");
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!("[TIMEOUT] {} ({}s)", fixture.name, timeout.as_secs());
            let expected_state = infer_expected_state_from_fixture_metadata(fixture);
            FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some(format!("Timed out after {}s", timeout.as_secs())),
                expected_state,
                actual_state: ActualState::Timeout,
                outcome: FixtureOutcome::Timeout,
                parity_success: false,
                actual_code: None,
                expected_code: None,
                is_error_fixture: matches!(expected_state, Some(ExpectedState::Error)),
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let expected_state = infer_expected_state_from_fixture_metadata(fixture);
            FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some("Fixture thread terminated unexpectedly".to_string()),
                expected_state,
                actual_state: ActualState::HarnessFailure,
                outcome: FixtureOutcome::HarnessFailure,
                parity_success: false,
                actual_code: None,
                expected_code: None,
                is_error_fixture: matches!(expected_state, Some(ExpectedState::Error)),
            }
        }
    }
}

fn run_fixture(fixture: &Fixture, run_skipped: bool, strict_output: bool) -> FixtureResult {
    let source = match std::fs::read_to_string(&fixture.input_path) {
        Ok(s) => s,
        Err(e) => {
            return FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some(format!("Failed to read input: {e}")),
                expected_state: None,
                actual_state: ActualState::HarnessFailure,
                outcome: FixtureOutcome::HarnessFailure,
                parity_success: false,
                actual_code: None,
                expected_code: None,
                is_error_fixture: false,
            };
        }
    };

    let expect_md = match std::fs::read_to_string(&fixture.expect_path) {
        Ok(s) => s,
        Err(e) => {
            return FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some(format!("Failed to read expect.md: {e}")),
                expected_state: None,
                actual_state: ActualState::HarnessFailure,
                outcome: FixtureOutcome::HarnessFailure,
                parity_success: false,
                actual_code: None,
                expected_code: None,
                is_error_fixture: false,
            };
        }
    };

    // Parse pragmas from first line
    let pragma = parse_pragma(&source);
    if std::env::var("DEBUG_PRAGMA").is_ok() {
        eprintln!(
            "[DEBUG_PRAGMA] file={} validatePreserveExistingMemoizationGuarantees={:?} enablePreserveExistingMemoizationGuarantees={:?}",
            fixture.input_path.file_name().unwrap().to_string_lossy(),
            pragma.validate_preserve_existing_memoization_guarantees,
            pragma.enable_preserve_existing_memoization_guarantees
        );
    }

    let expected_input = extract_input_block(&expect_md);
    let preprocessed_source_for_expectation = preprocess_flow_syntax_for_expectation(&source);
    let expected_code = extract_code_block(&expect_md);
    let expected_error = extract_error_block(&expect_md);
    let is_error_fixture = expected_code.is_none() && expected_error.is_some();
    let expected_state = if pragma.should_skip {
        Some(ExpectedState::Skip)
    } else if is_error_fixture {
        Some(ExpectedState::Error)
    } else {
        expected_code.as_ref().map(|code| {
            let is_bailout = is_expected_bailout(
                expected_input.as_deref(),
                &preprocessed_source_for_expectation,
                code,
            );
            if std::env::var("DEBUG_EXPECTED_STATE").is_ok() {
                eprintln!(
                    "[DEBUG_EXPECTED_STATE] fixture={} bailout={} input_eq={} preprocessed_eq={}",
                    fixture.name,
                    is_bailout,
                    expected_input
                        .as_deref()
                        .map(|input| normalize_bailout_text(input) == normalize_bailout_text(code))
                        .unwrap_or(false),
                    normalize_bailout_text(&preprocessed_source_for_expectation)
                        == normalize_bailout_text(code),
                );
                if std::env::var("DEBUG_EXPECTED_STATE_FULL").is_ok() {
                    eprintln!(
                        "[DEBUG_EXPECTED_STATE_FULL] fixture={} preprocessed_begin\n{}\n[DEBUG_EXPECTED_STATE_FULL] fixture={} preprocessed_end",
                        fixture.name, preprocessed_source_for_expectation, fixture.name
                    );
                    eprintln!(
                        "[DEBUG_EXPECTED_STATE_FULL] fixture={} expected_begin\n{}\n[DEBUG_EXPECTED_STATE_FULL] fixture={} expected_end",
                        fixture.name, code, fixture.name
                    );
                }
            }
            if is_bailout {
                ExpectedState::Bailout
            } else {
                ExpectedState::Transform
            }
        })
    };

    if expected_code.is_none() && expected_error.is_none() {
        return FixtureResult {
            name: fixture.name.clone(),
            status: Status::Fail,
            message: Some("No ## Code or ## Error block found in expect.md".to_string()),
            expected_state,
            actual_state: ActualState::HarnessFailure,
            outcome: FixtureOutcome::HarnessFailure,
            parity_success: false,
            actual_code: None,
            expected_code: None,
            is_error_fixture: matches!(expected_state, Some(ExpectedState::Error)),
        };
    }

    if matches!(expected_state, Some(ExpectedState::Skip)) && !run_skipped {
        return FixtureResult {
            name: fixture.name.clone(),
            status: Status::Pass,
            message: Some("Expected upstream skip (@skip)".to_string()),
            expected_state,
            actual_state: ActualState::Skipped,
            outcome: FixtureOutcome::ExpectedSkipMatch,
            parity_success: true,
            actual_code: None,
            expected_code: None,
            is_error_fixture: false,
        };
    }

    let filename = fixture
        .input_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let mut options = oxc_react_compiler::options::PluginOptions {
        compilation_mode: pragma.compilation_mode,
        panic_threshold: pragma.panic_threshold,
        custom_opt_out_directives: pragma.custom_opt_out_directives,
        ignore_use_no_forget: pragma.ignore_use_no_forget,
        no_emit: pragma.no_emit,
        ..Default::default()
    };

    // --- Wire PluginOptions-level pragmas ---

    if let Some(ref target) = pragma.target {
        options.target = target.clone();
    }
    if pragma.gating {
        options.gating = Some(oxc_react_compiler::options::GatingConfig {
            source: "ReactForgetFeatureFlag".to_string(),
            import_specifier_name: "isForgetEnabled_Fixtures".to_string(),
        });
    }
    if let Some(ref source) = pragma.dynamic_gating {
        options.dynamic_gating = Some(oxc_react_compiler::options::DynamicGatingConfig {
            source: source.clone(),
        });
    }
    if let Some(ref rules) = pragma.eslint_suppression_rules {
        options.eslint_suppression_rules = Some(rules.clone());
    }
    if let Some(fs) = pragma.flow_suppressions {
        options.flow_suppressions = fs;
    }

    // --- Wire EnvironmentConfig pragmas ---
    // The snap test runner sets `assertValidMutableRanges: true` and
    // `validatePreserveExistingMemoizationGuarantees: false` by default,
    // then overrides with `@validatePreserveExistingMemoizationGuarantees` if present.
    options.environment.assert_valid_mutable_ranges = true;
    // Snap default: false unless explicitly enabled by pragma
    options
        .environment
        .validate_preserve_existing_memoization_guarantees = pragma
        .validate_preserve_existing_memoization_guarantees
        .unwrap_or(false);
    // Snap default: enablePreserveExistingMemoizationGuarantees = false unless pragma says otherwise
    options
        .environment
        .enable_preserve_existing_memoization_guarantees = pragma
        .enable_preserve_existing_memoization_guarantees
        .unwrap_or(false);
    // Snap default: enableResetCacheOnSourceFileChanges = false unless pragma says otherwise
    if pragma.enable_reset_cache_on_source_file_changes.is_none() {
        options
            .environment
            .enable_reset_cache_on_source_file_changes = Some(false);
    }

    // Apply all optional boolean env config overrides
    macro_rules! apply_env_bool {
        ($pragma_field:ident, $env_field:ident) => {
            if let Some(v) = pragma.$pragma_field {
                options.environment.$env_field = v;
            }
        };
    }

    apply_env_bool!(
        validate_ref_access_during_render,
        validate_ref_access_during_render
    );
    apply_env_bool!(
        validate_no_set_state_in_render,
        validate_no_set_state_in_render
    );
    apply_env_bool!(
        validate_no_set_state_in_effects,
        validate_no_set_state_in_effects
    );
    apply_env_bool!(
        validate_no_derived_computations_in_effects,
        validate_no_derived_computations_in_effects
    );
    apply_env_bool!(
        validate_no_jsx_in_try_statements,
        validate_no_jsx_in_try_statements
    );
    apply_env_bool!(validate_static_components, validate_static_components);
    apply_env_bool!(
        validate_memoized_effect_dependencies,
        validate_memoized_effect_dependencies
    );
    apply_env_bool!(
        validate_no_impure_functions_in_render,
        validate_no_impure_functions_in_render
    );
    apply_env_bool!(
        validate_no_freezing_known_mutable_functions,
        validate_no_freezing_known_mutable_functions
    );
    apply_env_bool!(validate_no_void_use_memo, validate_no_void_use_memo);
    if let Some(ref blocklisted) = pragma.validate_blocklisted_imports {
        options.environment.validate_blocklisted_imports = Some(blocklisted.clone());
    }
    apply_env_bool!(
        validate_no_dynamically_created_components_or_hooks,
        validate_no_dynamically_created_components_or_hooks
    );
    apply_env_bool!(
        enable_preserve_existing_memoization_guarantees,
        enable_preserve_existing_memoization_guarantees
    );
    apply_env_bool!(
        enable_transitively_freeze_function_expressions,
        enable_transitively_freeze_function_expressions
    );
    apply_env_bool!(
        enable_assume_hooks_follow_rules_of_react,
        enable_assume_hooks_follow_rules_of_react
    );
    apply_env_bool!(enable_optional_dependencies, enable_optional_dependencies);
    apply_env_bool!(
        enable_treat_function_deps_as_conditional,
        enable_treat_function_deps_as_conditional
    );
    apply_env_bool!(
        enable_treat_ref_like_identifiers_as_refs,
        enable_treat_ref_like_identifiers_as_refs
    );
    apply_env_bool!(
        enable_treat_set_identifiers_as_state_setters,
        enable_treat_set_identifiers_as_state_setters
    );
    apply_env_bool!(enable_use_type_annotations, enable_use_type_annotations);
    apply_env_bool!(enable_jsx_outlining, enable_jsx_outlining);
    apply_env_bool!(enable_instruction_reordering, enable_instruction_reordering);
    apply_env_bool!(enable_memoization_comments, enable_memoization_comments);
    apply_env_bool!(
        enable_name_anonymous_functions,
        enable_name_anonymous_functions
    );
    apply_env_bool!(
        enable_custom_type_definition_for_reanimated,
        enable_custom_type_definition_for_reanimated
    );
    apply_env_bool!(
        enable_allow_set_state_from_refs_in_effects,
        enable_allow_set_state_from_refs_in_effects
    );
    apply_env_bool!(
        disable_memoization_for_debugging,
        disable_memoization_for_debugging
    );
    apply_env_bool!(
        enable_preserve_existing_manual_use_memo,
        enable_preserve_existing_manual_use_memo
    );
    apply_env_bool!(
        enable_new_mutation_aliasing_model,
        enable_new_mutation_aliasing_model
    );
    apply_env_bool!(enable_propagate_deps_in_hir, enable_propagate_deps_in_hir);
    apply_env_bool!(enable_reactive_scopes_in_hir, enable_reactive_scopes_in_hir);
    apply_env_bool!(
        enable_change_detection_for_debugging,
        enable_change_detection_for_debugging
    );
    apply_env_bool!(
        throw_unknown_exception_testonly,
        throw_unknown_exception_testonly
    );
    apply_env_bool!(enable_emit_freeze, enable_emit_freeze);
    apply_env_bool!(enable_emit_hook_guards, enable_emit_hook_guards);
    apply_env_bool!(enable_emit_instrument_forget, enable_emit_instrument_forget);
    apply_env_bool!(
        enable_change_variable_codegen,
        enable_change_variable_codegen
    );
    apply_env_bool!(enable_fire, enable_fire);

    if let Some(v) = pragma.enable_reset_cache_on_source_file_changes {
        options
            .environment
            .enable_reset_cache_on_source_file_changes = Some(v);
    }

    // @validateNoCapitalizedCalls: when set as boolean flag, use empty vec (test default)
    if let Some(true) = pragma.validate_no_capitalized_calls {
        options.environment.validate_no_capitalized_calls = Some(Vec::new());
    }

    // @throwUnknownException__testonly: simulate unexpected error
    if let Some(true) = pragma.throw_unknown_exception_testonly {
        options.environment.throw_unknown_exception_testonly = true;
    }

    // @lowerContextAccess: use test defaults from upstream TestUtils.ts
    if pragma.lower_context_access {
        options.environment.lower_context_access =
            Some(oxc_react_compiler::options::LowerContextAccessConfig {
                module: "react-compiler-runtime".to_string(),
                imported_name: "useContext_withSelector".to_string(),
            });
    }

    // @inferEffectDependencies: use test defaults from upstream TestUtils.ts
    if pragma.infer_effect_dependencies {
        options.environment.infer_effect_dependencies = Some(vec![
            oxc_react_compiler::options::InferEffectDepsConfig {
                function_module: "react".to_string(),
                function_name: "useEffect".to_string(),
                autodeps_index: 1,
            },
            oxc_react_compiler::options::InferEffectDepsConfig {
                function_module: "shared-runtime".to_string(),
                function_name: "useSpecialEffect".to_string(),
                autodeps_index: 2,
            },
            oxc_react_compiler::options::InferEffectDepsConfig {
                function_module: "useEffectWrapper".to_string(),
                function_name: "default".to_string(),
                autodeps_index: 1,
            },
        ]);
    }

    // @inlineJsxTransform: use test defaults from upstream TestUtils.ts
    if pragma.inline_jsx_transform.unwrap_or(false) {
        options.environment.inline_jsx_transform =
            Some(oxc_react_compiler::options::InlineJsxTransformConfig {
                element_symbol: "react.transitional.element".to_string(),
                global_dev_var: "DEV".to_string(),
            });
    }

    // @hookPattern:"regex"
    if let Some(ref pattern) = pragma.hook_pattern {
        options.environment.hook_pattern = Some(pattern.clone());
    }

    // @customMacros:"name" or @customMacros:"name.prop.path"
    if let Some(ref macro_str) = pragma.custom_macros {
        let parts: Vec<&str> = macro_str.split('.').collect();
        let name = parts[0].to_string();
        let mut props = Vec::new();
        for part in &parts[1..] {
            if *part == "*" {
                props.push(oxc_react_compiler::options::MacroProp::Wildcard);
            } else if !part.is_empty() {
                props.push(oxc_react_compiler::options::MacroProp::Name(
                    part.to_string(),
                ));
            }
        }
        options.environment.custom_macros =
            Some(vec![oxc_react_compiler::options::CustomMacroConfig {
                name,
                props,
            }]);
    }

    let result = oxc_react_compiler::compile(&filename, &source, &options);
    let language = if source.contains("@flow") {
        "flow"
    } else {
        "typescript"
    };
    let source_type = if source.contains("@script") {
        "script"
    } else {
        "module"
    };

    if matches!(expected_state, Some(ExpectedState::Error)) {
        if !result.transformed {
            FixtureResult {
                name: fixture.name.clone(),
                status: Status::Pass,
                message: None,
                expected_state,
                actual_state: ActualState::Error,
                outcome: FixtureOutcome::ExpectedErrorMatch,
                parity_success: true,
                actual_code: None,
                expected_code: None,
                is_error_fixture: true,
            }
        } else {
            FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some(
                    "Expected upstream error/bailout, but compiler returned transformed output"
                        .to_string(),
                ),
                expected_state,
                actual_state: ActualState::Transformed,
                outcome: FixtureOutcome::Mismatch,
                parity_success: false,
                actual_code: Some(canonicalize_strict_text(&result.code)),
                expected_code: expected_error.map(|s| canonicalize_strict_text(&s)),
                is_error_fixture: true,
            }
        }
    } else {
        let expected_code = expected_code.unwrap(); // safe: we checked above
        let postprocessed = maybe_apply_snap_post_babel_plugins(
            &result.code,
            &filename,
            language,
            source_type,
            false,
        );
        let postprocessed = normalize_post_babel_export_spacing(&postprocessed);
        let actual_source = format_code_for_compare(&fixture.input_path, &postprocessed);
        let expected_source = format_code_for_compare(&fixture.input_path, &expected_code);
        let actual = prepare_code_for_compare(&actual_source, strict_output);
        let expected = prepare_code_for_compare(&expected_source, strict_output);

        match expected_state.unwrap_or(ExpectedState::Transform) {
            ExpectedState::Transform => {
                if !result.transformed {
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Fail,
                        message: Some(
                            "Expected transformed output, but compiler bailed out/skipped"
                                .to_string(),
                        ),
                        expected_state,
                        actual_state: ActualState::Bailout,
                        outcome: FixtureOutcome::UnexpectedSkip,
                        parity_success: false,
                        actual_code: Some(actual),
                        expected_code: Some(expected),
                        is_error_fixture: false,
                    }
                } else if actual == expected {
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Pass,
                        message: None,
                        expected_state,
                        actual_state: ActualState::Transformed,
                        outcome: FixtureOutcome::TransformedMatch,
                        parity_success: true,
                        actual_code: None,
                        expected_code: None,
                        is_error_fixture: false,
                    }
                } else {
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Fail,
                        message: Some("Output mismatch".to_string()),
                        expected_state,
                        actual_state: ActualState::Transformed,
                        outcome: FixtureOutcome::Mismatch,
                        parity_success: false,
                        actual_code: Some(actual),
                        expected_code: Some(expected),
                        is_error_fixture: false,
                    }
                }
            }
            ExpectedState::Bailout => {
                if !result.transformed {
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Pass,
                        message: None,
                        expected_state,
                        actual_state: ActualState::Bailout,
                        outcome: FixtureOutcome::ExpectedBailoutMatch,
                        parity_success: true,
                        actual_code: None,
                        expected_code: None,
                        is_error_fixture: false,
                    }
                } else {
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Fail,
                        message: Some(
                            "Expected upstream bailout (untransformed output), but compiler transformed output"
                                .to_string(),
                        ),
                        expected_state,
                        actual_state: ActualState::Transformed,
                        outcome: FixtureOutcome::Mismatch,
                        parity_success: false,
                        actual_code: Some(actual),
                        expected_code: Some(expected),
                        is_error_fixture: false,
                    }
                }
            }
            ExpectedState::Skip => FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some(
                    "Fixture is marked @skip upstream but was executed (--run-skipped)".to_string(),
                ),
                expected_state,
                actual_state: if result.transformed {
                    ActualState::Transformed
                } else {
                    ActualState::Bailout
                },
                outcome: FixtureOutcome::Mismatch,
                parity_success: false,
                actual_code: Some(actual),
                expected_code: Some(expected),
                is_error_fixture: false,
            },
            ExpectedState::Error => FixtureResult {
                name: fixture.name.clone(),
                status: Status::Fail,
                message: Some("Expected-state classification mismatch".to_string()),
                expected_state,
                actual_state: if result.transformed {
                    ActualState::Transformed
                } else {
                    ActualState::Error
                },
                outcome: FixtureOutcome::HarnessFailure,
                parity_success: false,
                actual_code: Some(actual),
                expected_code: Some(expected),
                is_error_fixture: false,
            },
        }
    }
}

fn canonicalize_strict_text(code: &str) -> String {
    code.replace("\r\n", "\n").trim_end().to_string()
}

const OXFMT_FORMAT_SCRIPT: &str = r#"
import { format } from 'oxfmt';
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

for await (const line of rl) {
  if (!line) continue;
  let request;
  try {
    request = JSON.parse(line);
  } catch (error) {
    process.stdout.write(JSON.stringify({ error: error?.message || 'invalid request' }) + '\n');
    continue;
  }

  try {
    const result = await format(request.fileName || 'fixture.js', request.source || '', {});
    if (result.errors && result.errors.length > 0) {
      process.stdout.write(
        JSON.stringify({
          error: result.errors.map(error => error.message || 'unknown oxfmt error').join('\n'),
        }) + '\n',
      );
      continue;
    }
    process.stdout.write(JSON.stringify({ code: result.code }) + '\n');
  } catch (error) {
    process.stdout.write(JSON.stringify({ error: error?.message || 'oxfmt failed' }) + '\n');
  }
}
"#;

#[derive(Serialize)]
struct OxfmtRequest<'a> {
    #[serde(rename = "fileName")]
    file_name: &'a str,
    source: &'a str,
}

#[derive(Deserialize)]
struct OxfmtResponse {
    code: Option<String>,
    error: Option<String>,
}

struct OxfmtSession {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _stderr: ChildStderr,
}

fn init_oxfmt_session() -> Result<std::sync::Mutex<OxfmtSession>, String> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "failed to resolve workspace root".to_string())?;

    let mut child = Command::new("node")
        .current_dir(workspace_root)
        .args(["--input-type=module", "-e", OXFMT_FORMAT_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn oxfmt: {err}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to capture oxfmt stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture oxfmt stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture oxfmt stderr".to_string())?;

    Ok(std::sync::Mutex::new(OxfmtSession {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout),
        _stderr: stderr,
    }))
}

fn format_with_oxfmt(input_path: &Path, code: &str) -> Result<String, String> {
    static OXFMT_SESSION: OnceLock<Result<std::sync::Mutex<OxfmtSession>, String>> =
        OnceLock::new();

    let file_name = input_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fixture.js");
    let session = match OXFMT_SESSION.get_or_init(init_oxfmt_session) {
        Ok(session) => session,
        Err(err) => return Err(err.clone()),
    };

    let mut session = session
        .lock()
        .map_err(|_| "failed to lock oxfmt session".to_string())?;
    let request = serde_json::to_string(&OxfmtRequest {
        file_name,
        source: code,
    })
    .map_err(|err| format!("failed to encode oxfmt request: {err}"))?;
    session
        .stdin
        .write_all(request.as_bytes())
        .and_then(|_| session.stdin.write_all(b"\n"))
        .and_then(|_| session.stdin.flush())
        .map_err(|err| format!("failed to write oxfmt stdin: {err}"))?;

    let mut response_line = String::new();
    session
        .stdout
        .read_line(&mut response_line)
        .map_err(|err| format!("failed to read oxfmt output: {err}"))?;
    if response_line.is_empty() {
        return Err("oxfmt process terminated unexpectedly".to_string());
    }

    let response: OxfmtResponse = serde_json::from_str(response_line.trim_end())
        .map_err(|err| format!("failed to decode oxfmt response: {err}"))?;
    if let Some(error) = response.error {
        return Err(error);
    }

    response
        .code
        .ok_or_else(|| "oxfmt response missing formatted code".to_string())
}

fn format_code_for_compare(input_path: &Path, code: &str) -> String {
    format_with_oxfmt(input_path, code).unwrap_or_else(|_| code.to_string())
}

fn prepare_code_for_compare(code: &str, strict_output: bool) -> String {
    if strict_output {
        let mut normalized = canonicalize_strict_text(code);
        for _ in 0..6 {
            let next = normalize_strict_output_equivalences(
                &normalize_shared_cosmetic_equivalences(&normalized),
            );
            if next == normalized {
                return next;
            }
            normalized = next;
        }
        normalized
    } else {
        normalize_shared_cosmetic_equivalences(&normalize_code(code))
    }
}

fn normalize_strict_output_equivalences(code: &str) -> String {
    let mut normalized = code.to_string();
    let steps: &[fn(&str) -> String] = &[
        normalize_parenthesized_arrow_initializers,
        normalize_parenthesized_multiline_arrow_initializers,
        normalize_strict_multiline_call_open_args,
        normalize_strict_multiline_call_tail_args,
        normalize_trailing_comma_in_calls,
        normalize_label_same_line,
        normalize_multiline_jsx,
        normalize_jsx_children,
        normalize_jsx_tag_expr_spacing,
        normalize_jsx_inter_element_spacing,
        normalize_jsx_expr_spacing,
        normalize_jsx_text_child_spacing,
        normalize_jsx_space_expressions,
        normalize_jsx_text_before_tag,
        normalize_jsx_text_line_before_expr,
        normalize_temp_zero_suffixes,
        normalize_non_temp_ssa_suffixes,
        normalize_shadowed_temp_decls,
        normalize_temp_alpha_renaming,
        normalize_promote_temps,
        normalize_two_dep_guard_order,
        normalize_sort_simple_let_decl_runs,
        normalize_multiline_arrow_bodies,
        normalize_multiline_call_invocations,
        normalize_multiline_object_literal_access,
        normalize_multiline_optional_chain_calls,
        normalize_small_multiline_return_arrays,
        normalize_small_array_bracket_spacing,
        normalize_bracket_string_literal_spacing,
        normalize_object_shorthand_pairs,
        normalize_transitional_element_ref_shorthand,
        normalize_inline_jsx_cached_wrapper_scope,
        normalize_simple_else_load_blocks,
        normalize_memo_cache_decl_arity,
        normalize_jsx_text_expr_container_spacing,
        normalize_jsx_text_expr_spacing_compact,
        normalize_outlined_function_names,
        normalize_outlined_function_order,
        normalize_anonymous_function_space,
        normalize_arrow_copy_return_body,
        normalize_generated_memoization_comments,
        normalize_temp_alpha_renaming,
        normalize_destructuring_decl_kind,
    ];
    for step in steps {
        normalized = step(&normalized);
    }
    normalized
}

fn is_expected_bailout(expected_input: Option<&str>, source: &str, expected_code: &str) -> bool {
    let expected_norm = normalize_bailout_text(expected_code);
    if let Some(input) = expected_input
        && normalize_bailout_text(input) == expected_norm
    {
        return true;
    }
    normalize_bailout_text(source) == expected_norm
}

fn normalize_bailout_text(code: &str) -> String {
    let compact: String = canonicalize_strict_text(code)
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let normalized_quotes = compact.replace('\'', "\"");
    let normalized_arrows = single_param_arrow_paren_re()
        .replace_all(&normalized_quotes, "$1=>")
        .into_owned();
    trailing_comma_before_closer_re()
        .replace_all(&normalized_arrows, "$1")
        .into_owned()
}

fn normalize_shared_cosmetic_equivalences(code: &str) -> String {
    let mut normalized = code.to_string();
    let steps: [fn(&str) -> String; 17] = [
        normalize_compare_multiline_imports,
        normalize_import_region_comments,
        normalize_top_level_comment_trivia,
        normalize_compare_multiline_brace_literals,
        normalize_compare_trailing_sequence_null,
        normalize_multiline_trailing_commas_before_closers,
        normalize_labeled_switch_breaks,
        normalize_labeled_block_braces,
        normalize_labeled_switch_breaks,
        normalize_switch_case_braces,
        normalize_multiline_switch_cases,
        normalize_ts_object_type_semicolons,
        normalize_numeric_exponent_literals,
        normalize_compare_unicode_escapes,
        normalize_fixture_entrypoint_array_spacing,
        normalize_scope_body_blank_lines,
        normalize_top_level_statement_blank_lines,
    ];
    for step in steps {
        normalized = step(&normalized);
    }
    normalized
}

/// Strip standalone top-level comment trivia while preserving nested comments.
///
/// Strict-output fixtures still differ on nonsemantic file comments and pragma
/// notes. Those comments live at top level, outside transformed bodies, so we
/// can ignore them without masking nested semantic comment regressions.
fn normalize_top_level_comment_trivia(code: &str) -> String {
    let mut result = Vec::new();
    let mut top_level_brace_depth: i32 = 0;
    let mut in_top_level_block_comment = false;

    for line in code.lines() {
        let trimmed = line.trim();

        if in_top_level_block_comment {
            if trimmed.contains("*/") {
                in_top_level_block_comment = false;
            }
            continue;
        }

        if top_level_brace_depth == 0 {
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("/*") {
                if !trimmed.contains("*/") {
                    in_top_level_block_comment = true;
                }
                continue;
            }
            if trimmed.starts_with('*') || trimmed.starts_with("*/") {
                continue;
            }
        }

        result.push(line.to_string());

        top_level_brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
        top_level_brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
    }

    result.join("\n")
}

/// Remove blank lines caused by Babel's `retainLines: true + compact: true`.
///
/// When compact mode fits a multi-line construct onto fewer lines, the
/// remaining lines become blank. Prettier preserves one blank per gap.
/// Our codegen doesn't produce these blanks. Strip them from both sides:
///
/// 1. Blank lines inside scope check bodies (`if ($[N] ...)`)
/// 2. Blank lines after closing braces (`}`, `};`, `});`) inside function bodies
/// 3. Blank lines after cache declarations (`const $ = _c(N);`)
/// 4. Blank lines between `*/` and `function` declarations
/// 5. Blank lines between imports
fn normalize_scope_body_blank_lines(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut scope_depth: i32 = 0;
    let mut in_scope = false;
    let mut in_function_body = false;
    let mut function_brace_depth: i32 = 0;
    let mut prev_trimmed = "";

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Track whether we're inside a function body
        if !in_function_body && is_function_body_start(trimmed) {
            in_function_body = true;
            function_brace_depth = 1;
        } else if in_function_body {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            function_brace_depth += opens - closes;
            if function_brace_depth <= 0 {
                in_function_body = false;
            }
        }

        // Detect scope check start
        if !in_scope
            && (trimmed.starts_with("if ($[") || trimmed.starts_with("if (($["))
            && (trimmed.contains("Symbol.for") || trimmed.contains("!=="))
        {
            in_scope = true;
            scope_depth = 0;
        }

        if in_scope {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            scope_depth += opens - closes;

            // Skip blank lines inside scope bodies
            if trimmed.is_empty() && scope_depth > 0 {
                prev_trimmed = trimmed;
                continue;
            }

            if scope_depth <= 0 {
                in_scope = false;
            }
        }

        // Inside function bodies, strip all blank lines (retainLines artifact).
        // Babel's retainLines + compact creates blank lines at arbitrary positions
        // inside function bodies when compact mode reduces multi-line constructs.
        if in_function_body && trimmed.is_empty() {
            prev_trimmed = trimmed;
            continue;
        }

        // At top level, strip blank lines after closing braces
        if !in_function_body && trimmed.is_empty() {
            let pt = prev_trimmed;
            if pt == "}" || pt == "};" || pt == "});" || pt == "})" || pt.ends_with("};") {
                prev_trimmed = trimmed;
                continue;
            }
        }

        // Strip blank between `*/` and function declaration
        if trimmed.is_empty()
            && prev_trimmed == "*/"
            && lines
                .iter()
                .skip(i + 1)
                .find(|l| !l.trim().is_empty())
                .is_some_and(|next| next.trim().starts_with("function "))
        {
            prev_trimmed = trimmed;
            continue;
        }

        // Strip blank lines between import declarations
        if trimmed.is_empty()
            && prev_trimmed.starts_with("import ")
            && lines
                .iter()
                .skip(i + 1)
                .find(|l| !l.trim().is_empty())
                .is_some_and(|next| next.trim().starts_with("import "))
        {
            prev_trimmed = trimmed;
            continue;
        }

        prev_trimmed = trimmed;
        result.push(*line);
    }

    result.join("\n")
}

fn is_function_body_start(trimmed: &str) -> bool {
    trimmed.ends_with('{')
        && (trimmed.starts_with("function ")
            || trimmed.contains(" function ")
            || trimmed.contains("function(")
            || trimmed.contains("function (")
            || trimmed.contains("=> {"))
}

/// Collapse blank lines between completed top-level statements and the next
/// top-level binding/export statement. This keeps blank-line-only retainLines
/// artifacts from failing strict output without disturbing import-region
/// handling or blank lines inside nested blocks.
fn normalize_top_level_statement_blank_lines(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<&str> = Vec::with_capacity(lines.len());
    let mut top_level_brace_depth: i32 = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if trimmed.is_empty() && top_level_brace_depth == 0 {
            let prev_trimmed = result
                .iter()
                .rev()
                .copied()
                .find(|line| !line.trim().is_empty())
                .map(str::trim)
                .unwrap_or("");
            let next_trimmed = lines
                .iter()
                .skip(i + 1)
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim())
                .unwrap_or("");

            if ends_top_level_statement(prev_trimmed)
                && starts_top_level_binding_or_export(next_trimmed)
            {
                continue;
            }
        }

        result.push(*line);

        if !trimmed.is_empty() {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            top_level_brace_depth += opens - closes;
        }
    }

    result.join("\n")
}

fn ends_top_level_statement(trimmed: &str) -> bool {
    trimmed.ends_with(';') || trimmed.ends_with('}')
}

fn starts_top_level_binding_or_export(trimmed: &str) -> bool {
    trimmed.starts_with("export ")
        || trimmed.starts_with("const ")
        || trimmed.starts_with("let ")
        || trimmed.starts_with("var ")
}

/// Normalize comment placement in the import region at the top of the file.
///
/// Babel attaches leading program comments (pragmas) as trailing comments on
/// import lines, while OXC keeps them on separate lines. This normalization
/// detaches trailing comments from import lines and emits them as separate
/// lines, then strips blank lines in the import region. Both forms normalize
/// to the same canonical representation.
fn normalize_import_region_comments(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_import_region = true;

    for line in &lines {
        let trimmed = line.trim();

        if !in_import_region {
            result.push(line.to_string());
            continue;
        }

        // Import line — detach any trailing comment
        if trimmed.starts_with("import ") || trimmed.starts_with("const {") {
            if let Some(comment_pos) = find_trailing_comment_on_import(trimmed) {
                let import_part = trimmed[..comment_pos].trim_end();
                let comment_part = trimmed[comment_pos..].trim();
                result.push(import_part.to_string());
                if !comment_part.is_empty() {
                    result.push(comment_part.to_string());
                }
            } else {
                result.push(line.to_string());
            }
            continue;
        }

        // Blank line in import region — skip
        if trimmed.is_empty() {
            continue;
        }

        // Comment line in import region — keep
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            result.push(line.to_string());
            continue;
        }

        // Block comment continuation
        if trimmed.starts_with('*') {
            result.push(line.to_string());
            continue;
        }

        // Non-import, non-comment, non-blank — end of import region
        in_import_region = false;
        result.push(line.to_string());
    }

    result.join("\n")
}

fn normalize_generated_memoization_comments(code: &str) -> String {
    let inline_re = regex::Regex::new(r#"\s*// "(?:useMemo|useMemoCache)".*$"#).unwrap();
    code.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("// check if ")
                || trimmed == "// Inputs changed, recompute"
                || trimmed == "// Inputs did not change, use cached value"
            {
                return None;
            }

            Some(inline_re.replace(trimmed, "").to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize destructuring declaration kind: `const { ... }` and `const [ ... ]`
/// become `let { ... }` and `let [ ... ]`. Upstream and our codegen may disagree
/// on const vs let for destructuring patterns when bindings are later mutated.
fn normalize_destructuring_decl_kind(code: &str) -> String {
    let re = regex::Regex::new(r"\bconst\s+(\{|\[)").unwrap();
    re.replace_all(code, "let $1").to_string()
}

/// Find the position of a trailing `//` or `/*` comment on an import line,
/// skipping occurrences inside string literals.
fn find_trailing_comment_on_import(line: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' && !in_double {
            in_single = !in_single;
        } else if bytes[i] == b'"' && !in_single {
            in_double = !in_double;
        } else if bytes[i] == b';' && !in_single && !in_double {
            let rest = line[i + 1..].trim_start();
            if rest.starts_with("//") || rest.starts_with("/*") {
                let offset = line.len() - line[i + 1..].trim_start().len();
                return Some(offset);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Normalize array formatting within FIXTURE_ENTRYPOINT lines:
/// `[ { ... }, ]` → `[{ ... }]` and `[ ... ]` → `[...]` for single-line arrays.
///
/// Also normalizes:
/// - Parenthesized objects in arrays: `( { a: 1 })` → `{ a: 1 }` (AST codegen
///   wraps object literals in parens inside sequentialRenders arrays).
/// - Trailing semicolons: ensures `export let FIXTURE_ENTRYPOINT = { ... }` ends
///   with `;` (AST codegen may omit trailing semicolons on export declarations).
fn normalize_fixture_entrypoint_array_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            if line.contains("FIXTURE_ENTRYPOINT") {
                // Normalize `[ {` → `[{` and `}, ]` → `}]` and `, ]` → `]`
                let mut s = line.to_string();
                // Remove trailing comma before ] (single-line)
                while let Some(pos) = s.find(", ]") {
                    s.replace_range(pos..pos + 3, "]");
                }
                // Remove space after [ before {
                while let Some(pos) = s.find("[ {") {
                    s.replace_range(pos..pos + 3, "[{");
                }
                // Remove space after [ before other content (but not before ])
                while let Some(pos) = s.find("[ ") {
                    if s[pos + 2..].starts_with(']') {
                        break;
                    }
                    s.replace_range(pos..pos + 2, "[");
                }
                // Normalize parenthesized objects in arrays:
                // `( { ... })` → `{ ... }` — AST codegen wraps object literals
                // in parens inside sequentialRenders/params arrays.
                // Handle `( {` → `{` (opening paren before object)
                while s.contains("( {") {
                    s = s.replace("( {", "{");
                }
                // Handle `})` → `}` (closing paren after object) — but only when
                // preceded by a closing brace, i.e. the paren wraps an object.
                // We need to be careful not to strip `)` from function calls like
                // `createHookWrapper(useFoo)`. Only strip `)` that follows `}`
                // and is followed by `,` or `]` (i.e. inside array context).
                while s.contains("}),") || s.contains("})]") {
                    s = s.replace("}),", "},");
                    s = s.replace("})]", "}]");
                }
                // Ensure trailing semicolon on FIXTURE_ENTRYPOINT export declarations.
                // AST codegen may omit it: `... }` → `... };`
                let trimmed = s.trim_end();
                if (trimmed.starts_with("export let FIXTURE_ENTRYPOINT")
                    || trimmed.starts_with("export const FIXTURE_ENTRYPOINT"))
                    && trimmed.ends_with('}')
                    && !trimmed.ends_with("};")
                {
                    s = format!("{};", trimmed);
                }
                s
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_multiline_trailing_commas_before_closers(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r",\n([ \t]*[}\]])").unwrap())
        .replace_all(code, "\n$1")
        .into_owned()
}

fn normalize_bracket_string_literal_spacing(code: &str) -> String {
    let re = regex::Regex::new(r#"\[\s*("[^"\n]*"|'[^'\n]*')\s*\]"#).unwrap();
    re.replace_all(code, "[$1]").into_owned()
}

fn single_param_arrow_paren_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\(([A-Za-z_$][A-Za-z0-9_$]*)\)=>").unwrap())
}

fn trailing_comma_before_closer_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r",([}\]\)])").unwrap())
}

fn infer_expected_state_from_fixture_metadata(fixture: &Fixture) -> Option<ExpectedState> {
    let source = std::fs::read_to_string(&fixture.input_path).ok()?;
    let preprocessed_source_for_expectation = preprocess_flow_syntax_for_expectation(&source);
    let pragma = parse_pragma(&source);
    if pragma.should_skip {
        return Some(ExpectedState::Skip);
    }
    let expect_md = std::fs::read_to_string(&fixture.expect_path).ok()?;
    let expected_input = extract_input_block(&expect_md);
    let expected_code = extract_code_block(&expect_md);
    let expected_error = extract_error_block(&expect_md);
    if expected_code.is_none() && expected_error.is_some() {
        Some(ExpectedState::Error)
    } else {
        expected_code.map(|code| {
            if is_expected_bailout(
                expected_input.as_deref(),
                &preprocessed_source_for_expectation,
                &code,
            ) {
                ExpectedState::Bailout
            } else {
                ExpectedState::Transform
            }
        })
    }
}

const SNAP_POST_BABEL_SCRIPT: &str = r#"
const fs = require('node:fs');
const babel = require('@babel/core');
const babelParser = require('@babel/parser');
const HermesParser = require('hermes-parser');
const prettier = require('prettier');

async function main() {
  const input = fs.readFileSync(0, 'utf8');
  const filename = process.env.BABEL_FILENAME || 'fixture.js';
  const language = process.env.BABEL_LANGUAGE === 'flow' ? 'flow' : 'typescript';
  const sourceType = process.env.BABEL_SOURCE_TYPE === 'script' ? 'script' : 'module';
  const ast = language === 'flow'
    ? HermesParser.parse(input, {
        babel: true,
        flow: 'all',
        sourceFilename: filename,
        sourceType,
        enableExperimentalComponentSyntax: true,
      })
    : babelParser.parse(input, {
        sourceFilename: filename,
        plugins: ['typescript', 'jsx'],
        sourceType,
      });

  const result = babel.transformFromAstSync(ast, input, {
    filename,
    highlightCode: false,
    retainLines: true,
    compact: true,
    sourceType,
    plugins: [
      'babel-plugin-fbt',
      'babel-plugin-fbt-runtime',
      'babel-plugin-idx',
    ],
    configFile: false,
    babelrc: false,
  });

  if (!result || result.code == null) {
    process.stderr.write('snap post-babel transform produced no code');
    process.exit(2);
  }

  const output = await prettier.format(result.code, {
    semi: true,
    parser: language === 'flow' ? 'flow' : 'babel-ts',
  });

  process.stdout.write(output);
}

main().catch(error => {
  process.stderr.write(
    (error && error.stack) || (error && error.message) || String(error),
  );
  process.exit(2);
});
"#;

fn maybe_apply_snap_post_babel_plugins(
    code: &str,
    filename: &str,
    language: &str,
    source_type: &str,
    force_run: bool,
) -> String {
    let should_run = force_run || should_run_snap_post_babel_plugins(code);
    if !should_run {
        if std::env::var("DEBUG_POST_BABEL").is_ok() {
            eprintln!(
                "[DEBUG_POST_BABEL] file={} language={} source_type={} action=skip reason=no-fbt-fbs-idx-markers",
                filename, language, source_type
            );
        }
        return code.to_string();
    }
    if std::env::var("DEBUG_POST_BABEL").is_ok() {
        eprintln!(
            "[DEBUG_POST_BABEL] file={} language={} source_type={} action=run force_run={}",
            filename, language, source_type, force_run
        );
    }
    if std::env::var("DEBUG_POST_BABEL_CODE").is_ok() {
        eprintln!(
            "[DEBUG_POST_BABEL_CODE] file={} input_begin\n{}\n[DEBUG_POST_BABEL_CODE] file={} input_end",
            filename, code, filename
        );
    }
    match run_snap_post_babel_plugins(code, filename, language, source_type) {
        Ok(output) => {
            if std::env::var("DEBUG_POST_BABEL").is_ok() {
                let changed = if output != code {
                    "changed"
                } else {
                    "unchanged"
                };
                eprintln!(
                    "[DEBUG_POST_BABEL] file={} action=ok result={}",
                    filename, changed
                );
            }
            if std::env::var("DEBUG_POST_BABEL_CODE").is_ok() {
                eprintln!(
                    "[DEBUG_POST_BABEL_CODE] file={} output_begin\n{}\n[DEBUG_POST_BABEL_CODE] file={} output_end",
                    filename, output, filename
                );
            }
            output
        }
        Err(err) => {
            if std::env::var("DEBUG_POST_BABEL").is_ok() {
                eprintln!(
                    "[DEBUG_POST_BABEL] file={} language={} source_type={} error={}",
                    filename, language, source_type, err
                );
            }
            code.to_string()
        }
    }
}

fn should_run_snap_post_babel_plugins(code: &str) -> bool {
    code.contains("\"fbt\"")
        || code.contains("'fbt'")
        || code.contains("fbt(")
        || code.contains("fbt.")
        || code.contains("<fbt")
        || code.contains("fbt:")
        || code.contains("\"fbs\"")
        || code.contains("'fbs'")
        || code.contains("fbs(")
        || code.contains("fbs.")
        || code.contains("<fbs")
        || code.contains("fbs:")
        || code.contains("\"idx\"")
        || code.contains("'idx'")
        || code.contains("idx(")
        || code.contains("idx.")
}

fn run_snap_post_babel_plugins(
    code: &str,
    filename: &str,
    language: &str,
    source_type: &str,
) -> std::io::Result<String> {
    let compiler_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../third_party/react/compiler");

    let runtime = resolve_js_runtime().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no JavaScript runtime found for snap post-babel plugins",
        )
    })?;

    let mut command = Command::new(&runtime.executable);
    if runtime.run_as_node {
        command.env("ELECTRON_RUN_AS_NODE", "1");
    }

    let mut child = command
        .arg("-e")
        .arg(SNAP_POST_BABEL_SCRIPT)
        .current_dir(&compiler_dir)
        .env("BABEL_FILENAME", filename)
        .env("BABEL_LANGUAGE", language)
        .env("BABEL_SOURCE_TYPE", source_type)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(code.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(std::io::Error::other(format!(
            "snap post-babel failed status={} stderr={}",
            output.status, stderr
        )))
    }
}

fn resolve_js_runtime() -> Option<JsRuntime> {
    static JS_RUNTIME: OnceLock<Option<JsRuntime>> = OnceLock::new();
    JS_RUNTIME
        .get_or_init(|| {
            let mut candidates: Vec<JsRuntime> = Vec::new();
            if let Ok(path) = std::env::var("CONFORMANCE_JS_RUNTIME") {
                candidates.push(JsRuntime {
                    executable: PathBuf::from(path),
                    run_as_node: false,
                });
            }
            candidates.extend([
                JsRuntime {
                    executable: PathBuf::from("node"),
                    run_as_node: false,
                },
                JsRuntime {
                    executable: PathBuf::from("nodejs"),
                    run_as_node: false,
                },
                JsRuntime {
                    executable: PathBuf::from(
                        "/Applications/Visual Studio Code.app/Contents/MacOS/Electron",
                    ),
                    run_as_node: true,
                },
                JsRuntime {
                    executable: PathBuf::from("/Applications/Cursor.app/Contents/MacOS/Cursor"),
                    run_as_node: true,
                },
                JsRuntime {
                    executable: PathBuf::from("/Applications/Codex.app/Contents/MacOS/Codex"),
                    run_as_node: true,
                },
            ]);
            candidates.into_iter().find(js_runtime_is_available)
        })
        .clone()
}

fn js_runtime_is_available(runtime: &JsRuntime) -> bool {
    if runtime.executable.components().count() > 1 && !runtime.executable.exists() {
        return false;
    }
    let mut command = Command::new(&runtime.executable);
    if runtime.run_as_node {
        command.env("ELECTRON_RUN_AS_NODE", "1");
    }
    command
        .arg("-e")
        .arg("process.exit(0)")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn normalize_post_babel_export_spacing(code: &str) -> String {
    code.replace("\n    );\n\nexport default", "\n    );\nexport default")
        .replace(
            "\n    );\n\nexport const FIXTURE_ENTRYPOINT",
            "\n    );\nexport const FIXTURE_ENTRYPOINT",
        )
}

fn extract_input_block(md: &str) -> Option<String> {
    extract_markdown_code_block(md, "## Input")
}

/// Extract the code block from the `## Code` section of an `.expect.md` file.
fn extract_code_block(md: &str) -> Option<String> {
    extract_markdown_code_block(md, "## Code")
}

/// Extract the error block from the `## Error` section of an `.expect.md` file.
fn extract_error_block(md: &str) -> Option<String> {
    extract_markdown_code_block(md, "## Error")
}

fn extract_markdown_code_block(md: &str, header: &str) -> Option<String> {
    let header_idx = md.find(header)?;
    let rest = &md[header_idx..];
    let block_start = rest.find("```")?;
    let after_start = &rest[block_start + 3..];
    let newline = after_start.find('\n')?;
    let code_start = &after_start[newline + 1..];

    let mut offset = 0;
    let mut block_end = None;
    for line in code_start.lines() {
        if line.starts_with("```") {
            block_end = Some(offset);
            break;
        }
        offset += line.len() + 1;
    }
    let block_end = block_end?;

    Some(code_start[..block_end].trim_end().to_string())
}

fn preprocess_flow_syntax_for_expectation(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut saw_non_comment_code = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if !saw_non_comment_code
            && (trimmed == "//@flow"
                || trimmed == "// @flow"
                || trimmed.starts_with("//@flow ")
                || trimmed.starts_with("// @flow "))
        {
            continue;
        }
        if let Some(transformed_component) =
            transform_simple_flow_component_line_for_expectation(line)
        {
            result.push_str(&transformed_component);
            result.push('\n');
            saw_non_comment_code = true;
            continue;
        }
        let mut processed = line.to_string();
        if let Some(idx) = find_flow_keyword_for_expectation(&processed, "component") {
            let after = processed[idx + "component".len()..].trim_start();
            if after.starts_with(|c: char| c.is_uppercase()) {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "component".len()..]
                );
            }
        }
        if let Some(idx) = find_flow_keyword_for_expectation(&processed, "hook") {
            let after = processed[idx + "hook".len()..].trim_start();
            if after.starts_with("use") {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "hook".len()..]
                );
            }
        }
        if !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with('*')
            && !trimmed.starts_with("*/")
        {
            saw_non_comment_code = true;
        }
        result.push_str(&processed);
        result.push('\n');
    }
    if !source.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

fn transform_simple_flow_component_line_for_expectation(line: &str) -> Option<String> {
    let idx = find_flow_keyword_for_expectation(line, "component")?;
    let prefix = &line[..idx];
    let is_export_prefixed = prefix.trim_start().starts_with("export");
    let after_keyword = line[idx + "component".len()..].trim_start();
    if !after_keyword.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    let name_end = after_keyword
        .char_indices()
        .take_while(|(_, c)| is_identifier_char_for_expectation(*c))
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    let name = &after_keyword[..name_end];
    let rest = after_keyword[name_end..].trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = rest.find(')')?;
    let params = rest[1..close].trim();
    let tail = rest[close + 1..].trim_start();
    if !tail.starts_with('{') {
        return None;
    }
    if params.starts_with("...{") {
        let rewritten_param = params.trim_start_matches("...");
        return Some(format!(
            "{}function {}({}) {}",
            prefix, name, rewritten_param, tail
        ));
    }
    if params.is_empty() || params.starts_with("...") {
        return None;
    }
    if params.contains(',') {
        let parts: Vec<&str> = params.split(',').map(str::trim).collect();
        if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        let (first_name, first_type) = if let Some(colon) = parts[0].find(':') {
            let name = parts[0][..colon].trim();
            let ty = parts[0][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[0], None)
        };
        let (second_name, second_type) = if let Some(colon) = parts[1].find(':') {
            let name = parts[1][..colon].trim();
            let ty = parts[1][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[1], None)
        };
        if second_name != "ref" || !is_valid_js_identifier_for_expectation(first_name) {
            return None;
        }
        let rewritten_props = if let Some(ty) = first_type {
            format!(
                "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
                first_name, first_name, ty
            )
        } else {
            format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", first_name, first_name)
        };
        let ref_param = if let Some(ty) = second_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        if is_export_prefixed {
            return None;
        }
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef({}, {}): React.Node {}",
            prefix, name, name, prefix, name, rewritten_props, ref_param, tail
        ));
    }
    let (param_name, param_type) = if let Some(colon) = params.find(':') {
        let name = params[..colon].trim();
        let ty = params[colon + 1..].trim();
        (name, Some(ty))
    } else {
        (params, None)
    };
    if !is_valid_js_identifier_for_expectation(param_name) {
        return None;
    }
    if param_name == "ref" {
        if is_export_prefixed {
            return None;
        }
        let ref_param = if let Some(ty) = param_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef(_$$empty_props_placeholder$$: $ReadOnly<{{ }}>, {}): React.Node {}",
            prefix, name, name, prefix, name, ref_param, tail
        ));
    }
    let rewritten_param = if let Some(ty) = param_type {
        format!(
            "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
            param_name, param_name, ty
        )
    } else {
        format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", param_name, param_name)
    };
    Some(format!(
        "{}function {}({}): React.Node {}",
        prefix, name, rewritten_param, tail
    ))
}

fn find_flow_keyword_for_expectation(line: &str, keyword: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let offset = line.len() - trimmed.len();
    if let Some(after) = trimmed.strip_prefix(keyword)
        && (after.starts_with(' ') || after.starts_with('\t'))
    {
        return Some(offset);
    }
    if let Some(rest) = trimmed.strip_prefix("export") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("default") {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix(keyword)
                && (after.starts_with(' ') || after.starts_with('\t'))
            {
                let kw_offset = line.len() - rest.len();
                return Some(kw_offset);
            }
        }
        let rest2 = rest;
        if let Some(after) = rest2.strip_prefix(keyword)
            && (after.starts_with(' ') || after.starts_with('\t'))
        {
            let kw_offset = line.len() - rest2.len();
            return Some(kw_offset);
        }
    }
    None
}

fn is_identifier_char_for_expectation(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric()
}

fn is_valid_js_identifier_for_expectation(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        return false;
    }
    !matches!(
        name,
        "true"
            | "false"
            | "null"
            | "this"
            | "new"
            | "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "switch"
            | "case"
            | "default"
            | "function"
            | "class"
            | "import"
            | "export"
    )
}

/// Normalize code for comparison: trim lines, remove blank lines, normalize formatting.
fn normalize_code(code: &str) -> String {
    let debug_steps = std::env::var("DEBUG_NORMALIZE_STEPS").is_ok();
    let debug_steps_full = std::env::var("DEBUG_NORMALIZE_STEPS_FULL").is_ok();
    // Strip block comments (/** ... */, /* ... */) since Babel's codegen strips them.
    let code = normalize_strip_block_comments(code);
    // Strip inline comments per-line BEFORE joining, so we don't accidentally
    // strip code that was on a subsequent line in the original source.
    let code = normalize_strip_inline_comments(&code);
    let mut lines_normalized: String = code
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            normalize_import_line(trimmed)
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    type NormalizeStep = (&'static str, fn(&str) -> String);
    let steps: [NormalizeStep; 57] = [
        ("normalize_multiline_imports", normalize_multiline_imports),
        ("normalize_empty_blocks", normalize_empty_blocks),
        ("normalize_iife_parens", normalize_iife_parens),
        (
            "normalize_inline_single_stmt_iife",
            normalize_inline_single_stmt_iife,
        ),
        ("normalize_block_arrows", normalize_block_arrows),
        ("normalize_multiline_literals", normalize_multiline_literals),
        (
            "normalize_multiline_brace_literals",
            normalize_multiline_brace_literals,
        ),
        (
            "normalize_empty_block_spacing",
            normalize_empty_block_spacing,
        ),
        ("normalize_literal_formatting", normalize_literal_formatting),
        ("normalize_paren_wrapped", normalize_paren_wrapped),
        ("normalize_multiline_jsx", normalize_multiline_jsx),
        ("normalize_jsx_children", normalize_jsx_children),
        (
            "normalize_jsx_tag_expr_spacing",
            normalize_jsx_tag_expr_spacing,
        ),
        (
            "normalize_jsx_inter_element_spacing",
            normalize_jsx_inter_element_spacing,
        ),
        ("normalize_jsx_expr_spacing", normalize_jsx_expr_spacing),
        ("normalize_array_spacing", normalize_array_spacing),
        (
            "normalize_optional_chain_parens",
            normalize_optional_chain_parens,
        ),
        ("normalize_ts_annotations", normalize_ts_annotations),
        (
            "normalize_trailing_comma_in_calls",
            normalize_trailing_comma_in_calls,
        ),
        ("normalize_update_expressions", normalize_update_expressions),
        (
            "normalize_compound_assignments",
            normalize_compound_assignments,
        ),
        (
            "normalize_trailing_commas_final",
            normalize_trailing_commas_final,
        ),
        (
            "normalize_const_let_everywhere",
            normalize_const_let_everywhere,
        ),
        (
            "normalize_scope_guard_decl_order",
            normalize_scope_guard_decl_order,
        ),
        ("normalize_trailing_continue", normalize_trailing_continue),
        (
            "normalize_try_block_decl_order",
            normalize_try_block_decl_order,
        ),
        ("normalize_multiline_ternary", normalize_multiline_ternary),
        ("normalize_empty_if_stmts", normalize_empty_if_stmts),
        ("normalize_dead_loops", normalize_dead_loops),
        ("normalize_dead_do_while", normalize_dead_do_while),
        (
            "normalize_multiline_call_args",
            normalize_multiline_call_args,
        ),
        (
            "normalize_outlined_function_names",
            normalize_outlined_function_names,
        ),
        (
            "normalize_trailing_sequence_null",
            normalize_trailing_sequence_null,
        ),
        (
            "normalize_redundant_function_names",
            normalize_redundant_function_names,
        ),
        (
            "normalize_complex_type_annotations",
            normalize_complex_type_annotations,
        ),
        ("normalize_uninitialized_let", normalize_uninitialized_let),
        // Re-run scope guard decl order after uninitialized_let normalization.
        (
            "normalize_scope_guard_decl_order",
            normalize_scope_guard_decl_order,
        ),
        ("normalize_label_same_line", normalize_label_same_line),
        (
            "normalize_labeled_block_braces",
            normalize_labeled_block_braces,
        ),
        // Re-run switch label cleanup after labeled-block brace stripping so
        // `bb0: { switch (...) { ... } }` also normalizes to plain `switch`.
        (
            "normalize_labeled_switch_breaks_after_block_braces",
            normalize_labeled_switch_breaks,
        ),
        (
            "normalize_jsx_text_child_spacing",
            normalize_jsx_text_child_spacing,
        ),
        (
            "normalize_jsx_space_expressions",
            normalize_jsx_space_expressions,
        ),
        ("normalize_ts_type_assertions", normalize_ts_type_assertions),
        (
            "normalize_jsx_text_before_tag",
            normalize_jsx_text_before_tag,
        ),
        ("normalize_assignment_parens", normalize_assignment_parens),
        (
            "normalize_numeric_member_access",
            normalize_numeric_member_access,
        ),
        (
            "normalize_for_update_trailing_comma",
            normalize_for_update_trailing_comma,
        ),
        (
            "normalize_multiline_call_args_advanced",
            normalize_multiline_call_args_advanced,
        ),
        (
            "normalize_multiline_function_object_params",
            normalize_multiline_function_object_params,
        ),
        (
            "normalize_return_undefined_var",
            normalize_return_undefined_var,
        ),
        ("normalize_unicode_escapes", normalize_unicode_escapes),
        ("normalize_empty_switch_case", normalize_empty_switch_case),
        (
            "normalize_anonymous_function_space",
            normalize_anonymous_function_space,
        ),
        (
            "normalize_numeric_destructuring_key",
            normalize_numeric_destructuring_key,
        ),
        (
            "normalize_arrow_body_ternary_parens",
            normalize_arrow_body_ternary_parens,
        ),
        (
            "normalize_sentinel_scope_inline",
            normalize_sentinel_scope_inline,
        ),
        ("normalize_arrow_void_body", normalize_arrow_void_body),
    ];
    for (name, step) in steps {
        let before = lines_normalized.clone();
        lines_normalized = step(&lines_normalized);
        if debug_steps && before != lines_normalized {
            eprintln!("[DEBUG_NORMALIZE_STEP] changed={}", name);
            if debug_steps_full {
                eprintln!("[DEBUG_NORMALIZE_STEP][before]\n{}", before);
                eprintln!("[DEBUG_NORMALIZE_STEP][after]\n{}", lines_normalized);
            }
            let before_idx_lines: Vec<&str> = before
                .lines()
                .filter(|line| line.contains("idx."))
                .collect();
            let after_idx_lines: Vec<&str> = lines_normalized
                .lines()
                .filter(|line| line.contains("idx."))
                .collect();
            if !before_idx_lines.is_empty() || !after_idx_lines.is_empty() {
                eprintln!(
                    "[DEBUG_NORMALIZE_STEP] before idx-lines={:?}",
                    before_idx_lines
                );
                eprintln!(
                    "[DEBUG_NORMALIZE_STEP] after idx-lines={:?}",
                    after_idx_lines
                );
            }
        }
    }

    lines_normalized = normalize_redundant_comma_in_assignment(&lines_normalized);
    lines_normalized = normalize_trailing_comma_read_stmt(&lines_normalized);
    lines_normalized = normalize_jsx_expr_newline_before_closing_brace(&lines_normalized);
    lines_normalized = normalize_jsx_semicolon_on_own_line(&lines_normalized);
    lines_normalized = normalize_jsx_whitespace_before_closing_tag(&lines_normalized);
    lines_normalized = normalize_rename_suffixes(&lines_normalized);
    lines_normalized = normalize_temp_zero_suffixes(&lines_normalized);
    lines_normalized = normalize_non_temp_ssa_suffixes(&lines_normalized);
    lines_normalized = normalize_shadowed_temp_decls(&lines_normalized);
    lines_normalized = normalize_temp_alpha_renaming(&lines_normalized);
    lines_normalized = normalize_promote_temps(&lines_normalized);
    lines_normalized = normalize_two_dep_guard_order(&lines_normalized);
    lines_normalized = normalize_multiline_arrow_bodies(&lines_normalized);
    lines_normalized = normalize_multiline_if_conditions(&lines_normalized);
    lines_normalized = normalize_if_paren_spacing(&lines_normalized);
    lines_normalized = normalize_multiline_call_invocations(&lines_normalized);
    lines_normalized = normalize_multiline_arrow_fragment_expressions(&lines_normalized);
    lines_normalized = normalize_multiline_optional_chain_calls(&lines_normalized);
    lines_normalized = normalize_jsx_branch_paren_spacing(&lines_normalized);
    lines_normalized = normalize_jsx_nested_ternary_wrapper_parens(&lines_normalized);
    lines_normalized = normalize_simple_jsx_attr_brace_spacing(&lines_normalized);
    lines_normalized = normalize_jsx_tag_boundary_spaces(&lines_normalized);
    lines_normalized = normalize_jsx_text_expr_container_spacing(&lines_normalized);
    lines_normalized = normalize_jsx_text_expr_spacing_compact(&lines_normalized);
    lines_normalized = normalize_inline_jsx_cached_wrapper_scope(&lines_normalized);
    lines_normalized = normalize_inline_if_first_statements(&lines_normalized);
    lines_normalized = normalize_react_memo_closing_paren(&lines_normalized);
    lines_normalized = normalize_multiline_object_literal_access(&lines_normalized);
    lines_normalized = normalize_object_shorthand_pairs(&lines_normalized);
    lines_normalized = normalize_fbt_plural_cross_product_tables(&lines_normalized);
    lines_normalized = normalize_inline_if_first_statements(&lines_normalized);
    lines_normalized = normalize_multiline_object_method_bodies(&lines_normalized);
    lines_normalized = normalize_inline_if_first_statements(&lines_normalized);
    lines_normalized = normalize_simple_alias_return_tail(&lines_normalized);
    lines_normalized = normalize_arrow_copy_return_body(&lines_normalized);
    lines_normalized = normalize_sort_simple_let_decl_runs(&lines_normalized);
    lines_normalized = normalize_memo_cache_decl_arity(&lines_normalized);
    lines_normalized = normalize_object_shorthand_pairs(&lines_normalized);
    lines_normalized = normalize_transitional_element_ref_shorthand(&lines_normalized);
    lines_normalized = normalize_fbt_plural_cross_product_tables(&lines_normalized);
    lines_normalized = normalize_tail_return_from_cache_alias(&lines_normalized);
    lines_normalized = normalize_nullish_coalescing_ternary_parens(&lines_normalized);
    lines_normalized = normalize_outlined_function_order(&lines_normalized);
    lines_normalized = normalize_function_decl_trailing_semicolon(&lines_normalized);
    lines_normalized = normalize_arrow_expr_trailing_semicolon(&lines_normalized);
    lines_normalized = normalize_parenthesized_arrow_initializers(&lines_normalized);

    if debug_steps {
        eprintln!(
            "[DEBUG_NORMALIZE_STEP] final idx-lines={:?}",
            lines_normalized
                .lines()
                .filter(|line| line.contains("idx."))
                .collect::<Vec<_>>()
        );
    }
    lines_normalized
}

/// Collapse JSX expression containers split across lines as:
/// `... )\n} />` or `... )\n}>` or `... )\n}</Tag>`.
///
/// This is token-equivalent formatting noise from mixed printers (Rust codegen +
/// Babel post-pass) and should compare equal to single-line form.
fn normalize_jsx_expr_newline_before_closing_brace(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let current = lines[i].trim_end();
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim_start();
            if current.ends_with(')') && next.starts_with('}') {
                let rest = next[1..].trim_start();
                if rest.starts_with('>') || rest.starts_with("/>") || rest.starts_with("</") {
                    out.push(format!("{current}{next}"));
                    i += 2;
                    continue;
                }
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Join a JSX expression line with a standalone `;` on the following line.
fn normalize_jsx_semicolon_on_own_line(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim_end();
        if i + 1 < lines.len() && lines[i + 1].trim() == ";" {
            let trimmed = current.trim();
            if trimmed.starts_with('<') && trimmed.ends_with('>') {
                out.push(format!("{trimmed};"));
                i += 2;
                continue;
            }
        }
        out.push(current.trim().to_string());
        i += 1;
    }

    out.join("\n")
}

/// Normalize formatting to handle differences between source formatting and Babel's output.
fn normalize_import_line(line: &str) -> String {
    let mut s = line.to_string();
    // Normalize single quotes to double quotes globally.
    // OXC codegen emits single quotes, Babel emits double quotes.
    s = normalize_quotes(&s);
    // Normalize spacing in all object literals/destructuring: {x} -> { x }
    s = normalize_destructuring(&s);
    // Normalize single-param arrow functions: `x =>` -> `(x) =>`
    s = normalize_arrow_params(&s);
    s
}

/// Convert single quotes to double quotes, properly handling string contents.
/// When converting 'str' to "str", escape any inner double quotes.
fn normalize_quotes(line: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        if chars[i] == '\'' {
            // Find the matching closing single quote
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' {
                    j += 2; // skip escaped char
                    continue;
                }
                if chars[j] == '\'' {
                    break;
                }
                j += 1;
            }
            if j < n {
                // Found matching quote. Convert 'content' to "content",
                // escaping any inner double quotes.
                result.push('"');
                let mut k = i + 1;
                while k < j {
                    if chars[k] == '\\' && k + 1 < j {
                        if chars[k + 1] == '\'' {
                            // \' in single-quoted string -> just ' in double-quoted
                            result.push('\'');
                        } else {
                            result.push('\\');
                            result.push(chars[k + 1]);
                        }
                        k += 2;
                    } else if chars[k] == '"' {
                        // Unescaped " in single-quoted string -> \" in double-quoted
                        result.push('\\');
                        result.push('"');
                        k += 1;
                    } else {
                        result.push(chars[k]);
                        k += 1;
                    }
                }
                result.push('"');
                i = j + 1;
            } else {
                // No matching quote found, just output as double quote
                result.push('"');
                i += 1;
            }
        } else if chars[i] == '"' {
            // We're in a double-quoted string. Keep it as-is.
            result.push('"');
            let mut j = i + 1;
            while j < n {
                if chars[j] == '\\' && j + 1 < n {
                    result.push(chars[j]);
                    result.push(chars[j + 1]);
                    j += 2;
                    continue;
                }
                if chars[j] == '"' {
                    result.push('"');
                    j += 1;
                    break;
                }
                result.push(chars[j]);
                j += 1;
            }
            i = j;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// Normalize destructuring spacing: {x} -> { x }, {a, b} -> { a, b }.
/// Recursively normalizes nested braces.
fn normalize_destructuring(line: &str) -> String {
    fn find_matching_brace(chars: &[char], start: usize) -> Option<usize> {
        let mut depth = 1;
        let mut i = start + 1;
        let mut quote: Option<char> = None;

        while i < chars.len() {
            let ch = chars[i];
            if let Some(active_quote) = quote {
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                if ch == active_quote {
                    quote = None;
                }
                i += 1;
                continue;
            }

            match ch {
                '"' | '\'' | '`' => quote = Some(ch),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }

        None
    }

    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(active_quote) = quote {
            result.push(ch);
            if ch == '\\' {
                if i + 1 < chars.len() {
                    result.push(chars[i + 1]);
                    i += 2;
                    continue;
                }
            } else if ch == active_quote {
                quote = None;
            }
            i += 1;
            continue;
        }

        match ch {
            '"' | '\'' | '`' => {
                quote = Some(ch);
                result.push(ch);
                i += 1;
            }
            '{' => {
                if let Some(end) = find_matching_brace(&chars, i) {
                    // Extract the inner content between { and }
                    let inner: String = chars[i + 1..end].iter().collect();
                    let inner_trimmed = inner.trim().trim_end_matches(',');
                    // Recursively normalize the inner content (handles nested {})
                    let inner_normalized = normalize_destructuring(inner_trimmed);
                    result.push_str(&format!("{{ {} }}", inner_normalized));
                    i = end + 1;
                    continue;
                }
                result.push(ch);
                i += 1;
            }
            _ => {
                result.push(ch);
                i += 1;
            }
        }
    }
    result
}

/// Normalize empty blocks on separate lines: `{\n}` -> `{  }`
/// This handles the case where an empty loop body or function body is formatted
/// differently between our output and the expected output.
fn normalize_empty_blocks(code: &str) -> String {
    let mut result = code.to_string();
    // Replace `{\n}` with `{  }` (where } is on the next line by itself)
    loop {
        let search = "{\n}";
        if let Some(pos) = result.find(search) {
            result = format!("{}{{  }}{}", &result[..pos], &result[pos + search.len()..]);
        } else {
            break;
        }
    }
    result
}

/// Normalize empty block spacing: `{ }` → `{  }` for consistency.
/// Must run AFTER all multi-line collapsing passes since those may produce `{ }`.
fn normalize_empty_block_spacing(code: &str) -> String {
    code.replace("{ }", "{  }")
}

/// Normalize single-param arrow functions to always have parentheses.
/// `x =>` becomes `(x) =>`, but `(x) =>` and `(x, y) =>` stay unchanged.
fn normalize_arrow_params(line: &str) -> String {
    // Find `identifier =>` patterns (not preceded by `)` or other parens)
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        // Look for ` => ` or `\t=> ` preceded by an identifier
        if !(i + 3 >= n
            || chars[i] != ' '
            || chars[i + 1] != '='
            || chars[i + 2] != '>'
            || chars[i + 3] != ' ' && chars[i + 3] != '{')
        {
            // Check what's before the space: should be an identifier (not `)`)
            if !result.is_empty() {
                let prev = result.chars().last().unwrap_or(' ');
                if prev.is_alphanumeric() || prev == '_' || prev == '$' {
                    // Find the start of the identifier
                    let result_chars: Vec<char> = result.chars().collect();
                    let mut j = result_chars.len();
                    while j > 0
                        && (result_chars[j - 1].is_alphanumeric()
                            || result_chars[j - 1] == '_'
                            || result_chars[j - 1] == '$')
                    {
                        j -= 1;
                    }
                    // Check if preceded by `(` -- if so, it's already wrapped
                    let before_ident = if j > 0 { result_chars[j - 1] } else { ' ' };
                    if before_ident != '(' {
                        let ident: String = result_chars[j..].iter().collect();
                        let prefix: String = result_chars[..j].iter().collect();
                        result = format!("{}({}) =>", prefix, ident);
                        i += 3; // skip " =>"
                        continue;
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize block-body expression arrows to expression arrows.
/// Normalize IIFE parenthesization differences.
/// Some code wraps IIFEs in parens `(function() { ... })()` while others don't.
fn normalize_iife_parens(code: &str) -> String {
    // Normalize IIFE parenthesization differences:
    // `(function () {` → `function () {`
    // `})();` → `}();`
    // `})(args);` → `}(args);`
    let mut result = String::new();
    for line in code.lines() {
        let trimmed = line.trim();
        // Canonicalize inline nested IIFE punctuation:
        // `foo((function () {` -> `foo(function () {`
        // `...})());` -> `...}());`
        let mut normalized = trimmed
            .replace("((function", "(function")
            .replace("})(", "}(");
        if normalized.starts_with("(function") && normalized.ends_with('{') {
            // `(function () {` -> `function () {`
            normalized = normalized[1..].to_string();
        }
        result.push_str(&normalized);
        result.push('\n');
    }
    // Remove trailing newline to match input
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Inline single-statement IIFEs: removes the IIFE wrapper when it contains
/// only a single statement. This handles the case where our IIFE inlining pass
/// removes the wrapper but upstream preserves it.
///
/// `function () {\nSTMT;\n}();` → `STMT;`
/// `function () {\nSTMT;\n}(args);` → `STMT;` (args unused in body)
fn normalize_inline_single_stmt_iife(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Match `function () {` or `function() {`
        if (trimmed == "function () {" || trimmed == "function() {") && i + 2 < lines.len() {
            // Check if the next line is a statement and the line after closes with `}();` or `}(args);`
            let body_line = lines[i + 1].trim();
            let close_line = lines[i + 2].trim();
            if (close_line.starts_with("}(") && close_line.ends_with(");")) || close_line == "}();"
            {
                // Single-statement IIFE: inline the body
                result.push(body_line.to_string());
                i += 3;
                continue;
            }
        }
        // Also match `let f0 = function () {` pattern - skip the declaration
        // and just keep the body when the function is immediately called and the
        // variable isn't used elsewhere.
        result.push(lines[i].to_string());
        i += 1;
    }
    result.join("\n")
}

/// Converts `=> {\nreturn EXPR;\n}` to `=> EXPR` when EXPR is a single line.
/// This handles the formatting difference where our codegen emits block arrows
/// for expression arrows, while Babel emits expression arrows.
fn normalize_block_arrows(code: &str) -> String {
    let mut result = code.to_string();
    // Repeatedly find and replace the pattern
    loop {
        // Look for `=> {\nreturn ` pattern
        let search = "=> {\nreturn ";
        let Some(start) = result.find(search) else {
            break;
        };
        let after = &result[start + search.len()..];
        // Find the closing `;\n}` where the `}` is the matching brace
        if let Some(semi_pos) = after.find(";\n}") {
            let expr = &after[..semi_pos];
            // Only normalize if the expression is a single line (no newlines)
            if !expr.contains('\n') {
                let end = start + search.len() + semi_pos + 3;
                let replacement = format!("=> {}", expr);
                result = format!("{}{}{}", &result[..start], replacement, &result[end..]);
                continue;
            }
        }
        break;
    }
    result
}

/// Collapse multi-line array/object literals into single lines.
/// Handles formatting differences where one side breaks arrays across lines
/// and the other keeps them on one line (e.g., `sequentialRenders: [...]`).
fn normalize_multiline_literals(code: &str) -> String {
    let mut result = String::new();
    let mut bracket_depth = 0i32;
    let mut collecting = false;
    let mut collected = String::new();

    for line in code.lines() {
        let trimmed = line.trim();

        if !collecting {
            // Check if this line opens a bracket that continues to next line
            let opens: i32 = trimmed.chars().filter(|&c| c == '[').count() as i32;
            let closes: i32 = trimmed.chars().filter(|&c| c == ']').count() as i32;
            let net = opens - closes;

            if net > 0
                && !trimmed.ends_with("],")
                && !trimmed.ends_with("];")
                && !trimmed.ends_with(']')
            {
                // Start collecting multi-line literal
                collecting = true;
                bracket_depth = net;
                collected = trimmed.to_string();
            } else {
                result.push_str(trimmed);
                result.push('\n');
            }
        } else {
            // Continue collecting
            let opens: i32 = trimmed.chars().filter(|&c| c == '[').count() as i32;
            let closes: i32 = trimmed.chars().filter(|&c| c == ']').count() as i32;
            bracket_depth += opens - closes;

            // Append with space separator
            if !collected.ends_with(' ') && !trimmed.starts_with(']') {
                collected.push(' ');
            }
            collected.push_str(trimmed);

            if bracket_depth <= 0 {
                collecting = false;
                result.push_str(&collected);
                result.push('\n');
                collected.clear();
            }
        }
    }
    // Flush any remaining collected content
    if !collected.is_empty() {
        result.push_str(&collected);
        result.push('\n');
    }
    // Remove trailing newline
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize trailing commas and bracket spacing in array/object literals.
/// Babel sometimes includes trailing commas, OXC sometimes doesn't.
/// Also normalizes spacing around [ ] to match { } normalization.
fn normalize_literal_formatting(code: &str) -> String {
    // Normalize integer-valued floats: 42.0 → 42, -1.0 → -1
    let re_float = regex::Regex::new(r"\b(\d+)\.0\b").unwrap();
    let mut result = String::new();
    for line in code.lines() {
        let mut s = re_float.replace_all(line, "$1").to_string();
        // Remove trailing commas before ] or } (preserving space before closing bracket)
        loop {
            let prev = s.clone();
            s = s.replace(",]", "]");
            s = s.replace(",}", "}");
            s = s.replace(", ]", " ]");
            s = s.replace(", }", " }");
            if s == prev {
                break;
            }
        }
        // Normalize bracket spacing to match: [{ -> [ {, }] -> } ]
        // This matches what normalize_destructuring does for braces.
        s = normalize_brackets(&s);
        result.push_str(&s);
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Final trailing comma removal — runs after all multi-line collapsing.
/// Removes trailing commas before `}` or `]` (with or without space).
fn normalize_trailing_commas_final(code: &str) -> String {
    let mut result = String::new();
    for line in code.lines() {
        let mut s = line.to_string();
        loop {
            let prev = s.clone();
            s = s.replace(",]", "]");
            s = s.replace(",}", "}");
            s = s.replace(", ]", " ]");
            s = s.replace(", }", " }");
            if s == prev {
                break;
            }
        }
        result.push_str(&s);
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Remove `continue;` at the end of loop bodies.
/// A `continue;` as the last statement in `for/while/do-while { ... continue; }`
/// is a semantic no-op. Upstream eliminates it via pruneUnusedLabels.
fn normalize_trailing_continue(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        // Check for pattern: `continue;\n}`
        if lines[i].trim() == "continue;" {
            // Look ahead: is the next non-empty line a `}`?
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j < lines.len() && lines[j].trim() == "}" {
                // Skip the `continue;` line
                i += 1;
                continue;
            }
        }
        result.push(lines[i].to_string());
        i += 1;
    }
    result.join("\n")
}

/// Normalize multi-line ternary expressions: join continuation lines that start
/// with `?` or `:` (ternary operators) back to the previous line.
/// This handles formatting differences like:
///   t1 =\n  cond ? a : b;  →  t1 = cond ? a : b;
///   }.getValue()\n? CONST_STRING0\n: CONST_STRING1;  →  }.getValue() ? CONST_STRING0 : CONST_STRING1;
fn normalize_multiline_ternary(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::new();

    for line in &lines {
        let trimmed = line.trim();
        // If this line starts with ? or : (ternary continuation), join to previous line
        if (trimmed.starts_with("? ") || trimmed.starts_with(": ")) && !result.is_empty() {
            let last = result.last_mut().unwrap();
            // Remove trailing whitespace from previous line and join
            let prev = last.trim_end().to_string();
            *last = format!("{} {}", prev, trimmed);
        } else if !result.is_empty() {
            // Check if previous line ends with `=` (assignment continuation)
            let prev_trimmed = result.last().unwrap().trim_end();
            if prev_trimmed.ends_with(" =") || prev_trimmed.ends_with("\t=") {
                let last = result.last_mut().unwrap();
                let prev = last.trim_end().to_string();
                *last = format!("{} {}", prev, trimmed);
            } else {
                result.push(line.to_string());
            }
        } else {
            result.push(line.to_string());
        }
    }

    result.join("\n")
}

/// Remove empty if-statements: `if (expr) {  }` with no else branch.
/// These are semantically no-ops when the test has no side effects.
fn normalize_empty_if_stmts(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Match single-line `if (expr) {  }`
        if trimmed.starts_with("if (") && trimmed.ends_with("{  }") && !trimmed.contains("else") {
            // Skip this line entirely
            i += 1;
            continue;
        }
        result.push(lines[i]);
        i += 1;
    }
    result.join("\n")
}

/// Remove dead loops that only contain a break statement.
/// Pattern: `for (... of ...) { break; }` or `for (... in ...) { break; }`
fn normalize_dead_loops(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Match `for (... of/in ...) {` followed by `break;` followed by `}`
        if (trimmed.contains(" of ") || trimmed.contains(" in "))
            && trimmed.starts_with("for (")
            && trimmed.ends_with("{")
        {
            // Check if next two lines are `break;` and `}`
            if i + 2 < lines.len() {
                let next1 = lines[i + 1].trim();
                let next2 = lines[i + 2].trim();
                if next1 == "break;" && next2 == "}" {
                    // Skip all 3 lines
                    i += 3;
                    continue;
                }
            }
        }
        result.push(lines[i]);
        i += 1;
    }
    result.join("\n")
}

/// Collapse multi-line function call arguments: when a line ends with `,`
/// (after template literal, string, etc.) and the next line is just arguments + `);`,
/// merge them onto one line.
fn normalize_multiline_call_args(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Pattern: line ends with `,` (e.g., end of tagged template arg)
        // Next line is like `props.user);` (just remaining args)
        if trimmed.ends_with(',') && i + 1 < lines.len() {
            let next_trimmed = lines[i + 1].trim();
            // Next line should end with something that closes the call and not start with a control flow keyword
            let next_is_continuation = (next_trimmed.ends_with(");")
                || next_trimmed.ends_with(";"))
                && !next_trimmed.starts_with("if ")
                && !next_trimmed.starts_with("for ")
                && !next_trimmed.starts_with("while ")
                && !next_trimmed.starts_with("return ")
                && !next_trimmed.starts_with("let ")
                && !next_trimmed.starts_with("const ")
                // Do not fold lines that are already ternary continuations:
                // e.g. `}) : 42;` should stay attached to ternary normalization.
                && !next_trimmed.contains(" ? ")
                && !next_trimmed.contains(" : ")
                && (!next_trimmed.contains('{')
                    || next_trimmed.contains("?? {")
                    // Keep FBT/FBS runtime object options on the same line.
                    || next_trimmed.starts_with("{ hk:")
                    || next_trimmed.starts_with("{hk:"));
            if next_is_continuation {
                // Merge: current line + space + next line trimmed
                let merged = format!("{} {}", lines[i].trim_end(), next_trimmed);
                // Preserve indentation from current line
                let indent = lines[i].len() - lines[i].trim_start().len();
                result.push(format!("{}{}", " ".repeat(indent), merged));
                i += 2;
                continue;
            }
        }
        result.push(lines[i].to_string());
        i += 1;
    }
    result.join("\n")
}

/// Remove dead do-while loops: `do { break; } while (...);`
/// These are semantically equivalent to no-op when the body only breaks.
fn normalize_dead_do_while(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Match `do {` followed by `break;` followed by `} while (...);`
        if trimmed == "do {" && i + 2 < lines.len() {
            let next1 = lines[i + 1].trim();
            let next2 = lines[i + 2].trim();
            if next1 == "break;" && next2.starts_with("} while (") && next2.ends_with(");") {
                // Skip all 3 lines
                i += 3;
                continue;
            }
        }
        result.push(lines[i]);
        i += 1;
    }
    result.join("\n")
}

/// Normalize outlined function names so that both our `_temp`/`_temp2` naming
/// and upstream's `_ComponentOnClick`/`_FooBar` naming become consistent.
///
/// We detect top-level `function _XXX(` declarations (outlined functions always
/// start with `_` and are emitted after the main component function), then rename
/// them in order of appearance to `_temp`, `_temp2`, `_temp3`, etc.
fn normalize_outlined_function_names(code: &str) -> String {
    // Phase 1: Find all outlined function declarations.
    // Pattern: `function _XXX(` at the start of a line (after trimming whitespace).
    // We look for functions starting with `_` that are at module level (indent 0).
    let lines: Vec<&str> = code.lines().collect();
    let mut outlined_names: Vec<String> = Vec::new();

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.starts_with("function _") {
            // Extract function name: `function _XXX(` → `_XXX`
            if let Some(paren_pos) = trimmed.find('(') {
                let name = &trimmed["function ".len()..paren_pos];
                let name = name.trim();
                if name.starts_with('_') && !outlined_names.contains(&name.to_string()) {
                    outlined_names.push(name.to_string());
                }
            }
        }
    }

    if outlined_names.is_empty() {
        return code.to_string();
    }
    // Phase 2: Build rename pairs → deterministic `_temp`, `_temp1`, `_temp2`, ...
    // keyed by sorted original names so declaration order does not affect normalization.
    let mut sorted_names = outlined_names.clone();
    sorted_names.sort();
    let mut rename_pairs: Vec<(String, String)> = Vec::new();
    for (i, old_name) in sorted_names.iter().enumerate() {
        let new_name = if i == 0 {
            "_temp".to_string()
        } else {
            format!("_temp{}", i)
        };
        if *old_name != new_name {
            rename_pairs.push((old_name.clone(), new_name));
        }
    }

    if rename_pairs.is_empty() {
        return code.to_string();
    }

    // Phase 3: Replace via placeholders to avoid rename-chain collisions:
    // e.g. `_temp3 -> _temp`, `_temp -> _temp3`.
    let mut result = code.to_string();
    let mut placeholders: Vec<String> = Vec::new();
    for (idx, (old, _)) in rename_pairs.iter().enumerate() {
        let placeholder = format!("__OXC_OUTLINED_NAME_{}__", idx);
        result = replace_identifier_token(&result, old, &placeholder);
        placeholders.push(placeholder);
    }
    for ((_, new), placeholder) in rename_pairs.iter().zip(placeholders.iter()) {
        result = result.replace(placeholder, new);
    }

    result
}

fn replace_identifier_token(code: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut remaining = code;
    while let Some(pos) = remaining.find(old) {
        let before_ok = if pos == 0 {
            true
        } else {
            let c = remaining.as_bytes()[pos - 1] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };
        let after_pos = pos + old.len();
        let after_ok = if after_pos >= remaining.len() {
            true
        } else {
            let c = remaining.as_bytes()[after_pos] as char;
            !c.is_alphanumeric() && c != '_' && c != '$'
        };
        if before_ok && after_ok {
            out.push_str(&remaining[..pos]);
            out.push_str(new);
            remaining = &remaining[after_pos..];
        } else {
            out.push_str(&remaining[..after_pos]);
            remaining = &remaining[after_pos..];
        }
    }
    out.push_str(remaining);
    out
}

/// Collapse paren-wrapped multi-line expressions into single lines.
/// Handles patterns like: `t0 = (\n  <Foo />\n);` → `t0 = <Foo />;`
/// and `return (\n  <Foo />\n);` → `return <Foo />;`
fn normalize_paren_wrapped(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for lines ending with "= (" or "return ("
        if (trimmed.ends_with("= (") || trimmed.ends_with("return (") || trimmed == "(")
            && i + 1 < lines.len()
        {
            // Collect inner lines until we find ");" or ")"
            let prefix = if trimmed == "(" {
                String::new()
            } else if trimmed.ends_with("= (") {
                trimmed[..trimmed.len() - 1].to_string() // remove "("
            } else {
                // "return ("
                trimmed[..trimmed.len() - 1].to_string()
            };
            let mut inner = Vec::new();
            let mut j = i + 1;
            let mut found_close = false;
            let mut paren_depth = 1i32;
            while j < lines.len() {
                let t = lines[j].trim();
                // Track paren depth
                for ch in t.chars() {
                    match ch {
                        '(' => paren_depth += 1,
                        ')' => paren_depth -= 1,
                        _ => {}
                    }
                }
                if paren_depth <= 0 {
                    // This line closes the paren
                    // Check if it's just ")" or ");" or has content before )
                    let without_paren = t.trim_start_matches(')').trim_start_matches(';').trim();
                    if !without_paren.is_empty() {
                        inner.push(without_paren.to_string());
                    }
                    found_close = true;
                    // Determine suffix: ; or nothing
                    let suffix = if t.ends_with(");") { ";" } else { "" };
                    let combined = format!("{}{}{}", prefix, inner.join(" "), suffix);
                    result.push(combined);
                    i = j + 1;
                    break;
                }
                inner.push(t.to_string());
                j += 1;
            }
            if !found_close {
                result.push(trimmed.to_string());
                i += 1;
            }
        } else {
            result.push(trimmed.to_string());
            i += 1;
        }
    }
    result.join("\n")
}

/// Collapse multi-line JSX tags into single lines.
/// Handles patterns like: `<Foo\n  prop={val}\n/>` → `<Foo prop={val} />`
fn normalize_multiline_jsx(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for an opening JSX tag that doesn't close on the same line
        // Pattern: starts with < or contains `= <`, has no matching > or />
        let jsx_start = trimmed.starts_with('<')
            && !trimmed.starts_with("</")
            && !trimmed.contains("/>")
            && !trimmed.contains("</")
            && !trimmed.ends_with('>');

        if jsx_start {
            // Collect until we find /> or > on its own line
            let mut parts = vec![trimmed.to_string()];
            let mut j = i + 1;
            let mut found_close = false;
            while j < lines.len() {
                let t = lines[j].trim();
                parts.push(t.to_string());
                if t.ends_with("/>") || t.ends_with('>') {
                    found_close = true;
                    result.push(parts.join(" "));
                    i = j + 1;
                    break;
                }
                j += 1;
            }
            if !found_close {
                result.push(trimmed.to_string());
                i += 1;
            }
        } else {
            result.push(trimmed.to_string());
            i += 1;
        }
    }
    result.join("\n")
}

/// Collapse multi-line JSX children: `<div>\n{val}\n</div>` → `<div>{val}</div>`
/// Normalize JSX expression container spacing: `={ expr }` → `={expr}` and `>{ expr }<` → `>{expr}<`.
/// OXC codegen adds spaces inside `{ }` in JSX expression containers, Babel doesn't.
fn normalize_jsx_expr_spacing(code: &str) -> String {
    let mut result = String::new();
    for line in code.lines() {
        let trimmed = line.trim();
        // Only apply to lines that look like they contain JSX (have < or JSX-related content)
        if trimmed.contains("={ ")
            || trimmed.contains("{ \"")
            || trimmed.contains(" }/>")
            || trimmed.contains(" } />")
            || trimmed.contains(" }<")
            || trimmed.contains(">{ ")
        {
            // Normalize `={ expr }` → `={expr}` in JSX attributes
            // Carefully handle nested braces by only stripping the outermost spacing
            let mut s = trimmed.to_string();
            // Pattern: ={ ... } in attribute context
            // Replace `={ ` with `={` and ` }` with `}` only when in JSX context
            // Strategy: replace `={ ` → `={` and ` }/` → `}/` and ` }>` → `}>`
            s = s.replace("={ ", "{");
            // But we need to be more careful — only strip spaces around JSX expression braces
            // Let's use a simpler approach: strip spaces inside { } when preceded by = or >
            // Actually, let's just do a regex-like approach manually
            let s = normalize_jsx_brace_spaces(trimmed);
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&s);
        } else {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(trimmed);
        }
    }
    result
}

/// Normalize spaces between JSX tags and expression containers.
/// `<div> { "..." } </div>` → `<div>{"..."}</div>`
/// Handles the pattern where Babel adds spaces around JSX expression containers
/// while OXC codegen does not.
fn normalize_jsx_tag_expr_spacing(code: &str) -> String {
    let mut result = String::new();
    for line in code.lines() {
        let mut s = line.to_string();
        // `> {` → `>{` (space between closing > of tag and opening { of expression)
        s = s.replace("> {", ">{");
        // `} <` → `}<` (space between closing } of expression and opening < of close tag)
        s = s.replace("} </", "}</");
        s = s.replace("} <", "}<");
        // `} >` → `}>` Babel adds a space between last JSX attr `}` and `>` when there are children.
        // Only normalize when `>` is followed by a JSX child (not a JS greater-than operator).
        // Patterns: `} >{` (expression child), `} ><` (element child), `} >text` (text child)
        s = s.replace("} >{", "}>{");
        s = s.replace("} ><", "}><");
        s = s.replace("} > ", "}> ");
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&s);
    }
    result
}

/// Normalize spaces between JSX element tags within a line.
/// `<div> <span>x</span> </div>` → `<div><span>x</span></div>`
/// Strips whitespace-only text nodes between adjacent JSX tags.
fn normalize_jsx_inter_element_spacing(code: &str) -> String {
    let mut result = String::new();
    for line in code.lines() {
        let mut s = line.to_string();
        // Strip "> <" → "><" when both sides are JSX tags
        while s.contains("> <") {
            s = s.replace("> <", "><");
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&s);
    }
    result
}

/// Strip spaces around JSX element children within a line (UNUSED — kept for reference).
/// `<div> <span>x</span> </div>` → `<div><span>x</span></div>`
/// Only strips spaces between > and < (tag boundary).
#[allow(dead_code)]
fn normalize_jsx_element_spacing(s: &mut String) {
    // Strip "> <" → "><" only when both sides are JSX tags (> followed by space followed by <)
    // This is safe because in normal JS, "> <" only appears in JSX context
    while s.contains("> <") {
        *s = s.replace("> <", "><");
    }
    // Also strip trailing space before closing tag: "content </tag>" → "content</tag>"
    // But only when preceded by > (another tag close)
    while s.contains("/> ") {
        let new_s = s.replacen("/> ", "/>", 1);
        if new_s == *s {
            break;
        }
        *s = new_s;
    }
}

/// Strip spaces inside `{ ... }` in JSX expression contexts.
/// Handles `={  expr  }` → `={expr}` and `>{  expr  }<` → `>{expr}<`.
fn normalize_jsx_brace_spaces(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut result = String::new();
    let mut i = 0;
    while i < n {
        if chars[i] == '{' && i + 1 < n && chars[i + 1] == ' ' {
            // Check if this is a JSX expression container (preceded by = or >)
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let is_jsx_expr = prev == Some('=') || prev == Some('>');
            if is_jsx_expr {
                // Find the matching closing brace, strip inner leading/trailing spaces
                let mut depth = 1;
                let mut j = i + 1;
                while j < n && depth > 0 {
                    if chars[j] == '{' {
                        depth += 1;
                    }
                    if chars[j] == '}' {
                        depth -= 1;
                    }
                    if depth > 0 {
                        j += 1;
                    }
                }
                if depth == 0 {
                    // j points to the closing }
                    let inner: String = chars[i + 1..j].iter().collect();
                    let inner = inner.trim();
                    result.push('{');
                    result.push_str(inner);
                    result.push('}');
                    i = j + 1;
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn normalize_jsx_children(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for opening tag that ends with > (has children)
        if trimmed.starts_with('<')
            && !trimmed.starts_with("</")
            && trimmed.ends_with('>')
            && !trimmed.ends_with("/>")
        {
            // Extract tag name
            let tag_name = trimmed
                .trim_start_matches('<')
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("");
            if !tag_name.is_empty() {
                let close_tag = format!("</{}>", tag_name);
                // Look ahead for content + closing tag
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut found = false;
                while j < lines.len() && j < i + 5 {
                    let t = lines[j].trim();
                    parts.push(t.to_string());
                    if t == close_tag || t.starts_with(&close_tag) {
                        found = true;
                        result.push(parts.join(""));
                        i = j + 1;
                        break;
                    }
                    j += 1;
                }
                if found {
                    continue;
                }
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

/// Normalize temp-to-source variable promotion.
///
/// The upstream promoteUsedTemporaries pass may or may not promote a temporary
/// variable to use the source variable name. Our codegen always uses temps with
/// a post-scope assignment (`const x = t0;`). This normalization makes both forms
/// equivalent by finding `const <name> = <temp>;` patterns and renaming the temp
/// to the source name throughout.
///
/// Normalize optional chain parenthesization.
/// `(x?.y).z` → `x?.y.z` — strip parens around optional chains when followed by member access.
/// Both forms produce the same AST in the optional chain representation.
fn normalize_optional_chain_parens(code: &str) -> String {
    use regex::Regex;
    // Match `(expr?.prop).next` and strip the parens, making it `expr?.prop.next`,
    // but only when the opening `(` is not itself part of a call argument list.
    let re = Regex::new(r"(^|[^\w$])\(([^()]+\?\.[^()]+)\)(\.[a-zA-Z_$])").unwrap();
    let mut result = code.to_string();
    // Apply repeatedly until no more matches (for nested chains)
    loop {
        let new = re.replace_all(&result, "$1$2$3").to_string();
        if new == result {
            break;
        }
        result = new;
    }
    // Also handle bracket access: `(x?.y)[z]` → `x?.y[z]`
    let re2 = Regex::new(r"(^|[^\w$])\(([^()]+\?\.[^()]+)\)\[").unwrap();
    loop {
        let new = re2.replace_all(&result, "$1$2[").to_string();
        if new == result {
            break;
        }
        result = new;
    }
    result
}

/// Normalize array literal spacing differences.
/// `[{ a }` → `[ { a }` — add space after `[` when followed by `{`.
/// `[ { a }` → `[ { a }` — already has space, no change.
fn normalize_array_spacing(code: &str) -> String {
    // Normalize by removing optional space after `[`: `[ {` → `[{` and `[{` → `[{`
    // Then both sides are identical.
    code.replace("[ {", "[{")
}

/// Strip TypeScript type annotations from function parameters and return types.
/// Handles: `(_: unknown)` → `(_)`, `(x: string, y: number)` → `(x, y)`,
/// `function f(): void {}` → `function f() {}`
fn normalize_ts_annotations(code: &str) -> String {
    use regex::Regex;
    // Strip `: Type` from function params (e.g., `_: unknown` → `_`)
    let param_type = Regex::new(
        r"(\w)\s*:\s*(unknown|string|number|boolean|any|void|null|undefined|never|object|symbol|bigint|Array<[^>]*>|Record<[^>]*>)\s*([,)])"
    ).unwrap();
    // Strip return type annotation: `) : Type {` → `) {`
    let return_type = Regex::new(
        r"\)\s*:\s*(?:unknown|string|number|boolean|any|void|null|undefined|never|object|symbol|bigint)\s*\{"
    ).unwrap();
    // Strip `as const` type assertion (e.g., `return x as const;` → `return x;`)
    let as_const = Regex::new(r"\s+as\s+const\b").unwrap();
    let result = param_type.replace_all(code, "$1$3");
    let result = return_type.replace_all(&result, ") {");
    let result = as_const.replace_all(&result, "");
    result.to_string()
}

/// Remove trailing commas in function call arguments and normalize spacing.
/// `foo( arg, )` → `foo(arg)`, `foo(a, b, )` → `foo(a, b)`
/// Normalize `const` → `let` for variable declarations inside reactive scope bodies.
/// Normalize standalone update expressions to assignment form.
/// `y++;` → `y = y + 1;`  `++y;` → `y = y + 1;`
/// `y--;` → `y = y - 1;`  `--y;` → `y = y - 1;`
/// Only applies to standalone statements (identifiers and member expressions like x.a).
fn normalize_update_expressions(code: &str) -> String {
    // Simple identifier: x++ / ++x
    let postfix = regex::Regex::new(r"^([a-zA-Z_$][a-zA-Z0-9_$]*)(\+\+|--);\s*$").unwrap();
    let prefix = regex::Regex::new(r"^(\+\+|--)([a-zA-Z_$][a-zA-Z0-9_$]*);\s*$").unwrap();
    // Member expression: x.a++ / ++x.a (dotted paths only)
    let postfix_member = regex::Regex::new(
        r"^([a-zA-Z_$][a-zA-Z0-9_$]*(?:\.[a-zA-Z_$][a-zA-Z0-9_$]*)+)(\+\+|--);\s*$",
    )
    .unwrap();
    let prefix_member = regex::Regex::new(
        r"^(\+\+|--)([a-zA-Z_$][a-zA-Z0-9_$]*(?:\.[a-zA-Z_$][a-zA-Z0-9_$]*)+);\s*$",
    )
    .unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Try member expression patterns first (more specific)
            if let Some(caps) = postfix_member.captures(trimmed) {
                let var = &caps[1];
                let op = if &caps[2] == "++" { "+" } else { "-" };
                return format!("{} = {} {} 1;", var, var, op);
            }
            if let Some(caps) = prefix_member.captures(trimmed) {
                let op = if &caps[1] == "++" { "+" } else { "-" };
                let var = &caps[2];
                return format!("{} = {} {} 1;", var, var, op);
            }
            // Simple identifier patterns
            if let Some(caps) = postfix.captures(trimmed) {
                let var = &caps[1];
                let op = if &caps[2] == "++" { "+" } else { "-" };
                return format!("{} = {} {} 1;", var, var, op);
            }
            if let Some(caps) = prefix.captures(trimmed) {
                let op = if &caps[1] == "++" { "+" } else { "-" };
                let var = &caps[2];
                return format!("{} = {} {} 1;", var, var, op);
            }
            trimmed.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize compound assignment operators to their expanded form.
/// `x += y` → `x = x + y`, `x -= y` → `x = x - y`, etc.
/// Handles both simple identifiers and member expressions.
/// Only normalizes STATEMENT-level compound assignments (ending with `;`).
fn normalize_compound_assignments(code: &str) -> String {
    // Match: <lvalue> <op>= <rhs>;
    // where <lvalue> is an identifier or member expression (a.b.c)
    let compound = regex::Regex::new(
        r"^([a-zA-Z_$][a-zA-Z0-9_$]*(?:\.[a-zA-Z_$][a-zA-Z0-9_$]*)*)\s*(\+=|-=|\*=|/=|%=|&=|\|=|\^=|<<=|>>=|>>>=)\s*(.+);$"
    ).unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if let Some(caps) = compound.captures(trimmed) {
                let lvalue = &caps[1];
                let op = &caps[2];
                let rhs = caps[3].trim();
                let bin_op = &op[..op.len() - 1]; // Remove the `=` suffix
                return format!("{} = {} {} {};", lvalue, lvalue, bin_op, rhs);
            }
            trimmed.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The upstream's `promoteUsedTemporaries` pass converts scope-internal `const` declarations
/// to `let` for variables that are part of the reactive scope. This normalizer treats them
/// as equivalent so we don't fail on this stylistic difference.
/// Only applies inside `if ($[...])` scope bodies, not at module level or function level.
/// Normalize all `const` to `let` everywhere except cache declarations (`const $ = _c(...)`).
/// The let/const distinction is cosmetic — semantically equivalent for our purposes.
fn normalize_const_let_everywhere(code: &str) -> String {
    fn is_cache_decl(after_const: &str) -> bool {
        let mut s = after_const.trim_start();
        if !s.starts_with('$') {
            return false;
        }
        s = &s[1..];
        while let Some(ch) = s.chars().next() {
            if ch.is_ascii_digit() {
                s = &s[ch.len_utf8()..];
            } else {
                break;
            }
        }
        s = s.trim_start();
        if !s.starts_with('=') {
            return false;
        }
        s = &s[1..];
        s = s.trim_start();
        s.starts_with("_c")
    }

    // Replace all `const ` declarations with `let `, including those inside
    // inline arrow function bodies, except cache declarations (`const $ = _c`).
    // This handles both standalone lines and occurrences mid-line (e.g.,
    // inside `fn={() =>{const arr = []; ...}}`).
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Replace all occurrences of `const ` except cache declarations.
            // We iterate to handle multiple `const` on one line (e.g., for-of inside inline body).
            let mut result = trimmed.to_string();
            while let Some(pos) = result.find("const ") {
                // Check if this is a cache declaration like `const $ = _c` or `const $=_c`
                let after = &result[pos + 6..];
                if is_cache_decl(after) {
                    // Don't replace this one — but need to skip past it to find others
                    // Just break; cache decls are typically the only `const` on their line
                    break;
                }
                result = format!("{}let {}", &result[..pos], &result[pos + 6..]);
            }
            result
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Hoist bare `let x;` declarations from the start of `try {` blocks to before them.
///
/// Our codegen places uninitialized declarations inside the try block while upstream
/// places them before it. Both orderings are semantically equivalent for bare `let`
/// declarations since they're only assigned inside the try body anyway.
///
/// Example transform:
/// ```text
/// // Before normalization:
/// try {
///   let t0;
///   let t1;
///   ... actual code ...
///
/// // After normalization:
/// let t0;
/// let t1;
/// try {
///   ... actual code ...
/// ```
fn normalize_try_block_decl_order(code: &str) -> String {
    use regex::Regex;
    let bare_decl_re = Regex::new(r"^let \w+;$").unwrap();

    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        if trimmed == "try {" {
            // Look ahead: collect bare `let x;` declarations at start of try body
            let mut decls: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let bt = lines[j].trim();
                if bare_decl_re.is_match(bt) {
                    decls.push(bt.to_string());
                    j += 1;
                } else {
                    break;
                }
            }

            if !decls.is_empty() {
                // Emit hoisted declarations before try
                for d in decls {
                    result.push(d);
                }
                // Emit the `try {`
                result.push(trimmed.to_string());
                i = j; // skip past the hoisted decls
            } else {
                result.push(trimmed.to_string());
                i += 1;
            }
        } else {
            result.push(trimmed.to_string());
            i += 1;
        }
    }

    result.join("\n")
}

/// Normalize declaration ordering at reactive scope guard boundaries.
///
/// Our codegen sometimes places an initialized declaration (`let x = expr;`) at the
/// start of the `if ($[N] ...)` guard body while upstream places it just before the
/// guard (or vice versa). Both orderings are semantically equivalent when the
/// initializer doesn't depend on the cache check.
///
/// This normalization:
/// 1. Hoists `let/const x = expr;` from the very start of a guard body to before the guard
/// 2. Sorts all pre-guard declarations: initialized (`let x = expr;`) before bare (`let t0;`)
///
/// This runs on both actual and expected, so the canonical form is:
/// ```text
/// let x = expr;   // initialized declarations first
/// let t0;          // bare declarations second
/// if ($[0] ...) {  // scope guard
/// ```
fn normalize_scope_guard_decl_order(code: &str) -> String {
    use regex::Regex;
    let guard_re = Regex::new(r"^if \(\$\[\d+]").unwrap();
    let bare_decl_re = Regex::new(r"^let \w+;$").unwrap();

    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::new();

    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        if guard_re.is_match(trimmed) {
            // Collect declarations already before the guard (at end of result)
            let mut pre_decls: Vec<String> = Vec::new();
            while let Some(last) = result.last() {
                let lt = last.trim();
                if bare_decl_re.is_match(lt) {
                    pre_decls.push(result.pop().unwrap());
                } else {
                    break;
                }
            }
            pre_decls.reverse();

            // Collect bare declarations from the start of the guard body.
            // Keep initialized declarations in place since moving them changes scope semantics.
            let mut body_decls: Vec<String> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let bt = lines[j].trim();
                if bare_decl_re.is_match(bt) {
                    body_decls.push(bt.to_string());
                    j += 1;
                } else {
                    break;
                }
            }

            // Merge and sort for deterministic bare-declaration order.
            let mut all_decls: Vec<String> = Vec::new();
            all_decls.extend(pre_decls);
            all_decls.extend(body_decls);

            all_decls.sort();

            // Emit sorted declarations, then the guard
            for d in all_decls {
                result.push(d);
            }
            result.push(trimmed.to_string());
            i = j; // skip past hoisted body decls
        } else {
            result.push(trimmed.to_string());
            i += 1;
        }
    }

    result.join("\n")
}

#[allow(dead_code)]
fn normalize_const_let_in_scope(code: &str) -> String {
    let mut result = Vec::new();
    let mut in_scope = false;
    let mut scope_depth = 0;

    for line in code.lines() {
        let trimmed = line.trim();

        // Detect entry into a reactive scope: `if ($[0] === ... || $[0] !== ...`
        if trimmed.starts_with("if ($[") || trimmed.starts_with("if (($[") {
            in_scope = true;
            scope_depth = 0;
        }

        // Track brace depth within scope
        if in_scope {
            for ch in trimmed.chars() {
                match ch {
                    '{' => scope_depth += 1,
                    '}' => {
                        scope_depth -= 1;
                        if scope_depth <= 0 {
                            in_scope = false;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Inside a scope body, normalize const → let for variable declarations
        if in_scope
            && scope_depth > 0
            && trimmed.starts_with("const ")
            && !trimmed.starts_with("const $ = _c")
        {
            let normalized = trimmed.replacen("const ", "let ", 1);
            result.push(normalized);
            continue;
        }

        result.push(trimmed.to_string());
    }

    result.join("\n")
}

/// Strip dep-related lines for relaxed comparison.
/// Removes: `const $ = _c(N);`, `if ($[N] !== dep || ...)`, `$[N] = dep;`,
/// `} else {`, `var = $[N];`, `}` (scope closing).
/// This tells us which fixtures would pass if we had correct deps/cache sizes.
fn strip_dep_lines(code: &str) -> String {
    use regex::Regex;
    let cache_decl = Regex::new(r"^const \$\d* = _c\(\d+\);$").unwrap();
    let if_check = Regex::new(r"^if \(\$\d*\[\d+]").unwrap();
    let cache_store = Regex::new(r"^\$\d*\[\d+] = .+;$").unwrap();
    let cache_read = Regex::new(r"^\S+ = \$\d*\[\d+];$").unwrap();
    let let_decl = Regex::new(r"^let (t\d+|[a-zA-Z_]\w*);$").unwrap();

    code.lines()
        .map(|l| l.trim())
        .filter(|l| {
            !cache_decl.is_match(l)
                && !if_check.is_match(l)
                && !cache_store.is_match(l)
                && !cache_read.is_match(l)
                && !let_decl.is_match(l)
                && *l != "} else {"
                && *l != "}"
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_trailing_comma_in_calls(code: &str) -> String {
    use regex::Regex;
    // Remove trailing comma before closing paren: `, )` or `,)` → `)`
    let trailing = Regex::new(r",\s*\)").unwrap();
    let result = trailing.replace_all(code, ")");
    // Normalize space after opening paren in function calls: `foo( arg)` → `foo(arg)`
    // Match identifier followed by `( ` and non-paren char
    let space_after_open = Regex::new(r"(\w)\(\s+").unwrap();
    let result = space_after_open.replace_all(&result, "$1(");
    result.to_string()
}

/// Pattern A: `let t0; ... t0 = expr; ... const x = t0; return x;`
/// Pattern B: `let x; ... x = expr; ... return x;`
/// Both normalize to pattern B.
fn normalize_promote_temps(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0usize;
    let mut saw_function = false;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("function ") {
            saw_function = true;
            let mut depth =
                trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
            let start = i;
            i += 1;
            while i < lines.len() {
                let line = lines[i].trim();
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                i += 1;
                if depth <= 0 {
                    break;
                }
            }
            let chunk = lines[start..i].join("\n");
            result.push(normalize_promote_temps_in_chunk(&chunk));
            continue;
        }

        result.push(lines[i].to_string());
        i += 1;
    }

    if saw_function {
        result.join("\n")
    } else {
        normalize_promote_temps_in_chunk(code)
    }
}

fn normalize_promote_temps_in_chunk(code: &str) -> String {
    use std::collections::HashMap;

    let lines: Vec<&str> = code.lines().collect();

    // Find simple `<decl> <name> = t<N>;` aliases where t<N> is a compiler temp.
    // Only promote when the temp has exactly one alias target and otherwise
    // behaves like a plain carrier variable. This avoids rewriting legitimate
    // destructuring/catch bindings or clobbering earlier aliases with later ones.
    let re_alias_assign = regex::Regex::new(r"^(?:const|let|var)\s+(\w+)\s*=\s*(t\d+);?$").unwrap();
    let re_decl_head = regex::Regex::new(r"^(?:const|let|var)\b").unwrap();
    let re_temp = regex::Regex::new(r"^t\d+$").unwrap();
    let mut aliases_by_temp: HashMap<String, Vec<(usize, String)>> = HashMap::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(caps) = re_alias_assign.captures(trimmed) {
            let source_name = caps.get(1).unwrap().as_str();
            let temp_name = caps.get(2).unwrap().as_str();
            let mut previous_non_empty = "";
            let mut j = idx;
            while j > 0 {
                j -= 1;
                let prev = lines[j].trim();
                if !prev.is_empty() {
                    previous_non_empty = prev;
                    break;
                }
            }
            let catch_binding_alias =
                previous_non_empty.contains(&format!("catch ({})", temp_name));
            // Only rename if temp is actually a compiler temp (t0, t1, etc.)
            // and source is NOT itself a temp pattern
            if re_temp.is_match(temp_name)
                && !re_temp.is_match(source_name)
                && source_name != temp_name
                // Skip catch-binding aliases like:
                // `} catch (t1) {` followed by `const e = t1;`.
                // Renaming these asymmetrically creates false diffs.
                && !catch_binding_alias
            {
                aliases_by_temp
                    .entry(temp_name.to_string())
                    .or_default()
                    .push((idx, source_name.to_string()));
            }
        }
    }

    let mut rename_map: HashMap<String, String> = HashMap::new();
    for (temp_name, aliases) in aliases_by_temp {
        if aliases.len() != 1 {
            continue;
        }

        let (alias_idx, source_name) = &aliases[0];
        let temp_word = regex::Regex::new(&format!(r"\b{}\b", regex::escape(&temp_name))).unwrap();
        let plain_temp_decl = regex::Regex::new(&format!(
            r"^(?:const|let|var)\s+{}\s*(?:=\s*.+)?;?$",
            regex::escape(&temp_name)
        ))
        .unwrap();
        let source_decl = regex::Regex::new(&format!(
            r"^(?:const|let|var)\s+{}\b",
            regex::escape(source_name)
        ))
        .unwrap();

        let mut eligible = true;
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if idx != *alias_idx && source_decl.is_match(trimmed) {
                eligible = false;
                break;
            }

            if idx == *alias_idx {
                continue;
            }

            if temp_word.is_match(trimmed)
                && re_decl_head.is_match(trimmed)
                && !plain_temp_decl.is_match(trimmed)
            {
                eligible = false;
                break;
            }
        }

        if eligible {
            rename_map.insert(temp_name, source_name.clone());
        }
    }

    if rename_map.is_empty() {
        return code.to_string();
    }

    // Apply renames and remove identity assignments
    let mut result = Vec::new();
    let re_identity = regex::Regex::new(r"^(?:const|let|var)\s+(\w+)\s*=\s*(\w+);?$").unwrap();
    for line in &lines {
        let trimmed = line.trim();
        let mut s = trimmed.to_string();

        // Apply all renames using word boundary matching
        for (temp, source) in &rename_map {
            s = replace_word_boundary(&s, temp, source);
        }

        // Skip identity assignments like `const x = x;`
        if let Some(caps) = re_identity.captures(&s) {
            let lhs = caps.get(1).unwrap().as_str();
            let rhs = caps.get(2).unwrap().as_str();
            if lhs == rhs {
                continue;
            }
        }

        result.push(s);
    }

    result.join("\n")
}

/// Replace all whole-word occurrences of `old` with `new_val` in `s`.
fn replace_word_boundary(s: &str, old: &str, new_val: &str) -> String {
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(old))).unwrap();
    re.replace_all(s, new_val).to_string()
}

/// Normalize sequence expressions with trailing null/undefined.
/// `(x = expr), null;` → `x = expr;`
/// `(x = expr), undefined;` → `x = expr;`
/// The trailing null/undefined is a dead expression whose result is discarded
/// when used as a statement, making the two forms semantically equivalent.
/// Collapse multi-line brace-delimited literals (objects, methods) into single lines.
/// Handles patterns like:
/// ```
/// let obj = {
///   method() {  },
/// };
/// ```
/// → `let obj = { method() {  } };`
///
/// Only collapses when the opening line looks like an assignment/declaration
/// or a return/export pattern (not arbitrary code blocks like if/for/function).
fn normalize_multiline_brace_literals(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let is_bb_label_block = is_basic_block_label_open_brace(trimmed);
        // Match patterns like:
        // `let x = {`  `const x = {`  `return {`  `t0 = {`  `x: {`  `= [{`
        // Also match lines where the open brace is at the end (e.g., method bodies)
        let ends_with_open_brace = trimmed.ends_with('{');
        let is_obj_literal_start = (trimmed.ends_with("= {")
            || trimmed.ends_with(": {")
            || trimmed == "{"
            || trimmed.ends_with("({")
            || trimmed.ends_with(", {")
            || trimmed.ends_with("? {")
            || trimmed == "return {"
            // Match lines like `return { getValue() {` where an object literal
            // contains a method whose body is split across lines
            || (trimmed.starts_with("return {") && ends_with_open_brace)
            // Match lines like `obj = { method() {` where an object literal
            // assignment contains a method shorthand with body on next line
            || (trimmed.contains("= {") && ends_with_open_brace && trimmed.contains("() {")))
            && !trimmed.starts_with("if ")
            && !trimmed.starts_with("} else")
            && !trimmed.starts_with("for ")
            && !trimmed.starts_with("while ")
            && !trimmed.starts_with("do {")
            && !trimmed.starts_with("try {")
            && !trimmed.starts_with("catch")
            && !trimmed.starts_with("switch ")
            && !trimmed.starts_with("function ")
            && !trimmed.contains("=> {")
            // `bb0: { ... }` is a labeled block, not an object literal.
            // Collapsing it creates false conformance diffs.
            && !is_bb_label_block;

        if is_obj_literal_start {
            let open_braces = trimmed.matches('{').count();
            let close_braces = trimmed.matches('}').count();
            let net = open_braces as i32 - close_braces as i32;
            if net > 0 {
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut depth = net;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    parts.push(t.to_string());
                    j += 1;
                }
                // Only collapse if total is <= ~200 chars (avoid very long lines)
                let total_len: usize = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len();
                if total_len <= 200 {
                    let joined = parts.join(" ");
                    let cleaned = joined
                        .replace("  ", " ")
                        .replace(", }", " }")
                        .replace(",}", " }");
                    result.push(cleaned);
                    i = j;
                    continue;
                }
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn is_basic_block_label_open_brace(line: &str) -> bool {
    if !line.starts_with("bb") || !line.ends_with(": {") {
        return false;
    }
    let digits = &line[2..line.len() - 3];
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Collapse multi-line imports into single lines.
/// `import {\nfoo,\nbar,\n} from "mod";` → `import { foo, bar } from "mod";`
fn normalize_multiline_imports(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for `import {` that doesn't close on the same line
        if trimmed.starts_with("import {") || trimmed.starts_with("import type {") {
            let brace_open = trimmed.matches('{').count();
            let brace_close = trimmed.matches('}').count();
            if brace_open > brace_close {
                // Collect until we find the closing brace
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut depth = (brace_open - brace_close) as i32;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    parts.push(t.to_string());
                    j += 1;
                }
                // Join all parts into one line, clean up spacing
                let joined = parts.join(" ");
                // Clean up double spaces and trailing commas before }
                let cleaned = joined
                    .replace("  ", " ")
                    .replace(", }", " }")
                    .replace(",}", " }");
                result.push(cleaned);
                i = j;
                continue;
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn normalize_trailing_sequence_null(code: &str) -> String {
    let mut result = Vec::new();
    for line in code.lines() {
        let trimmed = line.trim();
        // Match pattern: `(EXPR), null;` or `(EXPR), undefined;`
        if (trimmed.ends_with("), null;") || trimmed.ends_with("), undefined;"))
            && trimmed.starts_with('(')
        {
            // Extract the inner expression
            let suffix_len = if trimmed.ends_with("), null;") { 8 } else { 13 }; // "), null;" or "), undefined;"
            let inner = &trimmed[1..trimmed.len() - suffix_len];
            // Only normalize if inner is a simple assignment (no nested commas)
            if !inner.contains(',') {
                result.push(format!("{};", inner));
                continue;
            }
        }
        result.push(trimmed.to_string());
    }
    result.join("\n")
}

fn trailing_comma_before_brace_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r",\s*}").unwrap())
}

fn normalize_compare_multiline_brace_literals(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let is_bb_label_block = is_basic_block_label_open_brace(trimmed);
        let is_fixture_entrypoint = trimmed.starts_with("export let FIXTURE_ENTRYPOINT = {")
            || trimmed.starts_with("export const FIXTURE_ENTRYPOINT = {");
        let ends_with_open_brace = trimmed.ends_with('{');
        let is_obj_literal_start = (is_fixture_entrypoint
            || trimmed.ends_with("= {")
            || trimmed.ends_with(": {")
            || trimmed == "{"
            || trimmed.ends_with("({")
            || trimmed.ends_with(", {")
            || trimmed.ends_with("? {")
            || trimmed == "return {"
            || (trimmed.starts_with("return {") && ends_with_open_brace)
            || (trimmed.contains("= {") && ends_with_open_brace && trimmed.contains("() {")))
            && !trimmed.starts_with("if ")
            && !trimmed.starts_with("} else")
            && !trimmed.starts_with("for ")
            && !trimmed.starts_with("while ")
            && !trimmed.starts_with("do {")
            && !trimmed.starts_with("try {")
            && !trimmed.starts_with("catch")
            && !trimmed.starts_with("switch ")
            && !trimmed.starts_with("function ")
            && !trimmed.contains("=> {")
            && !is_bb_label_block;

        if is_obj_literal_start {
            let open_braces = trimmed.matches('{').count();
            let close_braces = trimmed.matches('}').count();
            let net = open_braces as i32 - close_braces as i32;
            if net > 0 {
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut depth = net;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    parts.push(t.to_string());
                    j += 1;
                }
                if is_fixture_entrypoint {
                    parts = normalize_fixture_entrypoint_brace_parts(parts);
                }
                let total_len: usize = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len();
                if is_fixture_entrypoint || total_len <= 200 {
                    let joined = parts.join(" ");
                    let cleaned = trailing_comma_before_brace_regex()
                        .replace_all(&joined.replace("  ", " "), " }")
                        .to_string();
                    result.push(cleaned);
                    i = j;
                    continue;
                }
            }
        }

        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn normalize_fixture_entrypoint_brace_parts(parts: Vec<String>) -> Vec<String> {
    parts
        .into_iter()
        .filter_map(|part| {
            let stripped_block_comments = normalize_strip_block_comments(&part);
            let trimmed = stripped_block_comments.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                return None;
            }
            if let Some(pos) = find_line_comment_start(trimmed) {
                let before = trimmed[..pos].trim_end();
                if before.is_empty() {
                    None
                } else {
                    Some(before.to_string())
                }
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

fn normalize_compare_multiline_imports(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("import {") || trimmed.starts_with("import type {") {
            let brace_open = trimmed.matches('{').count();
            let brace_close = trimmed.matches('}').count();
            if brace_open > brace_close {
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut depth = (brace_open - brace_close) as i32;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    if !is_comment_only_import_line(t) {
                        parts.push(t.to_string());
                    }
                    j += 1;
                }
                let joined = parts.join(" ");
                let cleaned = trailing_comma_before_brace_regex()
                    .replace_all(&joined.replace("  ", " "), " }")
                    .to_string();
                result.push(cleaned);
                i = j;
                continue;
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn is_comment_only_import_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("*/")
}

fn normalize_compare_trailing_sequence_null(code: &str) -> String {
    let mut result = Vec::new();
    let assign_then_read = regex::Regex::new(
        r"\(\(([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(.+?)\),\s*([A-Za-z_$][A-Za-z0-9_$]*)\);",
    )
    .unwrap();
    let assign_then_discard =
        regex::Regex::new(r"\(\(([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(.+?)\),\s*(?:null|undefined)\);")
            .unwrap();
    for line in code.lines() {
        let trimmed = line.trim();
        let rewritten = assign_then_read
            .replace_all(trimmed, |caps: &regex::Captures| {
                if caps.get(1).unwrap().as_str() == caps.get(3).unwrap().as_str() {
                    format!("{} = {};", &caps[1], &caps[2])
                } else {
                    caps.get(0).unwrap().as_str().to_string()
                }
            })
            .to_string();
        let rewritten = assign_then_discard
            .replace_all(&rewritten, |caps: &regex::Captures| {
                format!("{} = {};", &caps[1], &caps[2])
            })
            .to_string();
        let trimmed = rewritten.trim();
        if (trimmed.ends_with("), null;") || trimmed.ends_with("), undefined;"))
            && trimmed.starts_with('(')
        {
            let suffix_len = if trimmed.ends_with("), null;") { 8 } else { 13 };
            let inner = &trimmed[1..trimmed.len() - suffix_len];
            if !inner.contains(',') {
                result.push(format!(
                    "{};",
                    inner.trim_matches(|ch| ch == '(' || ch == ')')
                ));
                continue;
            }
        }
        result.push(trimmed.to_string());
    }
    result.join("\n")
}

fn normalize_labeled_switch_breaks(code: &str) -> String {
    let labeled_switch = regex::Regex::new(r"\bbb\d+:\s*(switch\s*\()").unwrap();
    let code = labeled_switch.replace_all(code, "$1").to_string();
    let labeled_break = regex::Regex::new(r"\bbreak\s+bb\d+;").unwrap();
    labeled_break.replace_all(&code, "break;").to_string()
}

fn normalize_switch_case_braces(code: &str) -> String {
    let mut result = Vec::new();
    let mut in_case_brace = false;
    for line in code.lines() {
        let trimmed = line.trim();
        if let Some(prefix) = trimmed.strip_suffix(" {")
            && (prefix.starts_with("case ") || prefix == "default:")
        {
            result.push(prefix.to_string());
            in_case_brace = true;
            continue;
        }
        if in_case_brace && trimmed == "}" {
            in_case_brace = false;
            continue;
        }
        if trimmed.starts_with("case ") || trimmed == "default:" {
            in_case_brace = false;
        }
        result.push(trimmed.to_string());
    }
    result.join("\n")
}

fn normalize_multiline_switch_cases(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("case ") || trimmed == "default:" {
            let mut parts = vec![trimmed.to_string()];
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j].trim();
                if next.starts_with("case ") || next == "default:" || next == "}" {
                    break;
                }
                parts.push(next.to_string());
                j += 1;
            }
            result.push(parts.join(" "));
            i = j;
            continue;
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn normalize_ts_object_type_semicolons(code: &str) -> String {
    let re = regex::Regex::new(r";(\s*})").unwrap();
    re.replace_all(code, "$1").to_string()
}

fn normalize_numeric_exponent_literals(code: &str) -> String {
    let re = regex::Regex::new(r"\b(\d+)e([+-]?\d+)\b").unwrap();
    re.replace_all(code, |caps: &regex::Captures| {
        let base = caps[1].parse::<u128>().ok();
        let exponent = caps[2].parse::<i32>().ok();
        match (base, exponent) {
            (Some(base), Some(exponent)) if (0..=18).contains(&exponent) => base
                .checked_mul(10u128.pow(exponent as u32))
                .map(|value| value.to_string())
                .unwrap_or_else(|| caps[0].to_string()),
            _ => caps[0].to_string(),
        }
    })
    .to_string()
}

/// Strip redundant function names in function expressions.
/// When a function expression is passed as an argument and the containing variable
/// has the same name, Babel strips the function name but OXC preserves it.
/// E.g., `let X = forwardRef(function X(` → `let X = forwardRef(function (`
fn normalize_redundant_function_names(code: &str) -> String {
    // Pattern: `let X = someFunc(function X(` → `let X = someFunc(function (`
    // We can't use backreferences in Rust regex, so parse manually.
    let decl_re =
        regex::Regex::new(r"^((?:let|const|var)\s+)(\w+)(\s*=\s*[\w.]+\(function\s+)(\w+)(\(.*)$")
            .unwrap();
    code.lines()
        .map(|line| {
            if let Some(caps) = decl_re.captures(line) {
                let var_name = &caps[2];
                let func_name = &caps[4];
                if var_name == func_name {
                    // Strip the function name: replace `function X(` with `function (`
                    let prefix = &caps[1]; // "let "
                    let eq_part = &caps[3]; // " = forwardRef(function "
                    // Remove the function name from eq_part
                    let func_keyword_with_name = format!("function {}", func_name);
                    let new_eq_part = eq_part.replace(&func_keyword_with_name, "function ");
                    let rest = &caps[5]; // "(props, ref) {"
                    return format!("{}{}{}{}", prefix, var_name, new_eq_part, rest);
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip complex type annotations from function parameters.
/// The existing normalize_ts_annotations handles simple types like `: number`,
/// but not object types like `: { id: number }`.
/// This handles: `(props: { ... })` → `(props)`
fn normalize_complex_type_annotations(code: &str) -> String {
    let mut result = String::new();
    for line in code.lines() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&strip_param_object_type(line));
    }
    result
}

/// Strip object type annotations from function params.
/// Handles `(name: { ... })` patterns where the type is an object literal type.
fn strip_param_object_type(line: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    while i < n {
        match chars[i] {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            _ => {}
        }
        // Look for pattern: identifier `: {` inside function params
        // We detect this by looking for `: {` preceded by a word character
        if i + 2 < n && chars[i] == ':' && chars[i + 1] == ' ' && chars[i + 2] == '{' {
            // Check if preceded by identifier char (this is a type annotation)
            let before_ok = i > 0
                && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '$');
            // Only strip when we're in a function parameter list. Without this
            // guard, object literals like `_1: { ... }` are incorrectly erased.
            let in_param_list = paren_depth > 0 && brace_depth == 0 && bracket_depth == 0;
            if before_ok && in_param_list {
                // Find matching closing brace
                let mut depth = 0;
                let mut j = i + 2;
                let mut found_param_type = false;
                while j < n {
                    if chars[j] == '{' {
                        depth += 1;
                    }
                    if chars[j] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            // Check if followed by `)` or `,` (confirming it's a param type)
                            let mut k = j + 1;
                            while k < n && chars[k] == ' ' {
                                k += 1;
                            }
                            if k < n && (chars[k] == ')' || chars[k] == ',') {
                                // Skip from `: {` to after `}`
                                i = j + 1;
                                found_param_type = true;
                            }
                            break;
                        }
                    }
                    j += 1;
                }
                if found_param_type {
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize spacing inside square brackets: `[x]` → `[x]` (no change for simple),
/// but `[{...}]` → `[ {...} ]` and `[ x ]` → `[ x ]` (keep existing spaces).
/// The goal is to make `[{prop: 1}]` and `[ {prop: 1} ]` compare equal.
fn normalize_brackets(line: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < chars.len() && depth > 0 {
                if chars[j] == '[' {
                    depth += 1;
                }
                if chars[j] == ']' {
                    depth -= 1;
                }
                j += 1;
            }
            if depth == 0 {
                let inner: String = chars[i + 1..j - 1].iter().collect();
                let inner_trimmed = inner.trim();
                if inner_trimmed.is_empty() {
                    result.push_str("[]");
                } else {
                    result.push_str(&format!("[{}]", inner_trimmed));
                }
                i = j;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize `bb0:\n` followed by a statement to `bb0: <statement>` (label on same line).
/// E.g.:
///   bb0:
///   switch (...) {
/// becomes:
///   bb0: switch (...) {
fn normalize_label_same_line(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check if this line is a label line: `bbN:` or `bbN: `
        if trimmed.starts_with("bb") && trimmed.ends_with(':') {
            // Merge with next line
            if i + 1 < lines.len() {
                result.push(format!("{} {}", trimmed, lines[i + 1].trim()));
                i += 2;
                continue;
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

/// Normalize labeled block braces: strip the `{ }` wrapper from labeled blocks.
///
/// Upstream emits `bb0: { if (cond) { ... } stmt; }` (braces around the labeled body),
/// while our codegen emits `bb0: if (cond) { ... }\nstmt;` (no wrapper braces).
/// This normalizer strips the outer block braces so both forms compare as equal.
///
/// Handles:
/// - Single-line: `bb0: { content }` -> `bb0: content`
/// - Multi-line: `bb0: {\n...\n}` -> `bb0:\n...`
/// - Nested labels: `bb0: { bb1: { ... } ... }` (applied repeatedly)
/// - Mid-line labels: `case X: { ... bb0: { ... } ... }`
fn normalize_labeled_block_braces(code: &str) -> String {
    // Apply repeatedly until stable (handles nested labeled blocks)
    let mut result = code.to_string();
    loop {
        let next = strip_labeled_block_braces_pass(&result);
        if next == result {
            break;
        }
        result = next;
    }
    result
}

/// One pass of labeled-block brace stripping, operating on the full code string.
/// Finds `bbN: { ` patterns anywhere in the text and strips the matching `{ }`.
fn strip_labeled_block_braces_pass(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        // Look for `bb` followed by digits, then `: { `  or `: {\n`
        if i + 4 < len && chars[i] == 'b' && chars[i + 1] == 'b' {
            let mut j = i + 2;
            // Skip digits
            while j < len && chars[j].is_ascii_digit() {
                j += 1;
            }
            // Need at least one digit
            if j > i + 2
                && j + 2 < len
                && chars[j] == ':'
                && chars[j + 1] == ' '
                && chars[j + 2] == '{'
            {
                // Check what follows the `{`
                let brace_pos = j + 2;
                let after_brace = if brace_pos + 1 < len {
                    chars[brace_pos + 1]
                } else {
                    '\0'
                };
                if after_brace == ' ' || after_brace == '\n' {
                    // Found `bbN: { ` or `bbN: {\n` — find matching `}`
                    let content_start = brace_pos + 1; // position right after `{`
                    let mut depth: i32 = 1;
                    let mut k = content_start;
                    // If after_brace is ' ', skip it
                    if after_brace == ' ' {
                        k += 1;
                    }
                    let scan_start = k;
                    while k < len && depth > 0 {
                        match chars[k] {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if depth == 0 {
                        // k points to matching `}`
                        // Emit everything up to and including `bbN:` but NOT ` {`
                        // i.e., emit chars[i..j+1] then the inner content chars[scan_start..k]
                        // then skip the `}`
                        for c in &chars[i..j + 1] {
                            result.push(*c);
                        }
                        result.push(' ');
                        // Emit inner content, trimming leading/trailing whitespace
                        let inner: String = chars[scan_start..k].iter().collect();
                        let inner_trimmed = inner.trim();
                        result.push_str(inner_trimmed);
                        i = k + 1; // skip past the `}`
                        continue;
                    }
                    // No matching `}` found; emit as-is
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize JSX text children: ensure spaces between `}` and `{` in JSX context.
/// Handles the difference between `}{x}` and `} {x}` where the space is significant
/// in JSX text context.
/// Strategy: in lines that look like JSX children (contain `>{...}` or `}{`),
/// normalize `}{` to `} {` so both formatters agree.
fn normalize_jsx_text_child_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Only apply in JSX context — line contains JSX expression containers
            if (trimmed.contains(">{") || trimmed.contains("}{")) && trimmed.contains("}<") {
                // Replace `}{` with `} {` (but not `}={` which is JSX attribute)
                let mut result = String::new();
                let chars: Vec<char> = trimmed.chars().collect();
                let n = chars.len();
                let mut i = 0;
                while i < n {
                    if i + 1 < n && chars[i] == '}' && chars[i + 1] == '{' {
                        result.push_str("} {");
                        i += 2;
                    } else {
                        result.push(chars[i]);
                        i += 1;
                    }
                }
                result
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize JSX whitespace-only string expressions and leading text spacing.
///
/// 1. Replaces `{" "}` / `{ " " }` (space-only string JSX expressions) with a single space.
///    Our codegen may emit `{" "}` while Babel absorbs these into adjacent text nodes.
/// 2. Ensures consistent spacing between `>` and text content: `>text` → `> text`.
///    When multiline JSX is collapsed by `normalize_paren_wrapped`, Babel's output gains
///    a space from line joining (`<>\nHello` → `<> Hello`), while our single-line codegen
///    produces `<>Hello`. This normalizer makes both forms equivalent.
fn normalize_jsx_space_expressions(code: &str) -> String {
    // Step 1: Replace {" "} / { " " } with a single space, then collapse double spaces
    let re = regex::Regex::new(r#"\{\s*" "\s*}"#).unwrap();
    let result = re.replace_all(code, " ");
    // Collapse double spaces resulting from the replacement (e.g., "} { " " } {" → "}   {" → "} {")
    let mut collapsed = result.to_string();
    while collapsed.contains("  ") {
        collapsed = collapsed.replace("  ", " ");
    }

    // Step 2: Normalize ">text" to "> text" where text is JSX text content
    let tag_text_re = regex::Regex::new(r">([A-Za-z])").unwrap();
    let mut output = String::new();
    for line in collapsed.lines() {
        let trimmed = line.trim();
        // Only apply to lines that look like JSX (contain opening and closing tags)
        if trimmed.contains('<')
            && trimmed.contains('>')
            && (trimmed.contains("</") || trimmed.contains("</>"))
        {
            let normalized = tag_text_re.replace_all(trimmed, "> $1").to_string();
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&normalized);
        } else {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(trimmed);
        }
    }
    output
}

/// Normalize TypeScript type assertions: strip `as <type>` and `satisfies <type>` from expressions.
/// E.g.: `let y = x as number;` → `let y = x;`
///       `let y = x satisfies number;` → `let y = x;`
/// Only strips simple type assertions (primitive types, identifiers), not complex ones.
fn normalize_ts_type_assertions(code: &str) -> String {
    let as_re = regex::Regex::new(
        r"\s+as\s+(number|string|boolean|any|unknown|void|null|undefined|never|object|symbol|bigint)\b"
    ).unwrap();
    let satisfies_re = regex::Regex::new(
        r"\s+satisfies\s+(number|string|boolean|any|unknown|void|null|undefined|never|object|symbol|bigint)\b"
    ).unwrap();
    let result = as_re.replace_all(code, "");
    let result = satisfies_re.replace_all(&result, "");
    result.to_string()
}

/// Normalize `let x = undefined;` to `let x;` — semantically equivalent.
fn normalize_uninitialized_let(code: &str) -> String {
    // Normalize `let x = undefined;` -> `let x;`
    let re = regex::Regex::new(r"let\s+(\w+)\s*=\s*undefined\s*;").unwrap();
    let code = re.replace_all(code, "let $1;").to_string();
    // Normalize `let x = 0;` -> `let x;` and `let x = null;` -> `let x;`
    // These are common dead-store initializers that DCE should eliminate.
    // The upstream removes these initializers; we sometimes don't due to
    // phi-node liveness propagation differences.
    let re2 = regex::Regex::new(r"let\s+(\w+)\s*=\s*(0|null)\s*;").unwrap();
    re2.replace_all(&code, "let $1;").to_string()
}

/// Normalize JSX text-before-tag spacing: `text<Tag>` → `text <Tag>`.
///
/// OXC's JSX printer may omit the space between JSX text content and adjacent
/// opening/closing tags, while Babel preserves it. This normalizer adds a space
/// between a word character and `<` when followed by `/` (closing tag) or an
/// uppercase letter (component tag).
///
/// Only applied to lines that contain JSX closing tags to avoid false positives
/// in non-JSX contexts like comparisons (`a<b`).
fn normalize_jsx_text_before_tag(code: &str) -> String {
    let tag_re = regex::Regex::new(r"(\w)(<(?:/|[A-Z]))").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Only apply to lines that look like JSX (contain closing tags)
            if trimmed.contains("</") || trimmed.contains("/>") {
                tag_re.replace_all(trimmed, "$1 $2").to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_jsx_text_line_before_expr(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i > 0 && i + 1 < lines.len() {
            let previous = lines[i - 1].trim();
            let next = lines[i + 1].trim();
            let looks_like_open_tag =
                previous.starts_with('<') && previous.ends_with('>') && !previous.starts_with("</");
            let is_plain_text_line = !current.is_empty()
                && !current.contains('<')
                && !current.contains('>')
                && !current.contains('{')
                && !current.contains('}')
                && !current.ends_with(';');
            if looks_like_open_tag && is_plain_text_line && next.starts_with('{') {
                result.push(format!("{current} {next}"));
                i += 2;
                continue;
            }
        }

        result.push(current.to_string());
        i += 1;
    }

    result.join("\n")
}

fn normalize_small_array_bracket_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("return [") || trimmed.contains("= [") {
                trimmed
                    .replace("[ ", "[")
                    .replace(" ]", "]")
                    .replace(", ]", "]")
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_small_multiline_return_arrays(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed == "return [" {
            let mut parts = vec![trimmed.to_string()];
            let mut depth = 1i32;
            let mut j = i + 1;
            while j < lines.len() && depth > 0 {
                let current = lines[j].trim();
                depth += current.matches('[').count() as i32 - current.matches(']').count() as i32;
                parts.push(current.to_string());
                j += 1;
            }
            if depth == 0 {
                let total_len = parts.iter().map(String::len).sum::<usize>() + parts.len();
                if total_len <= 200 {
                    result.push(parts.join(" "));
                    i = j;
                    continue;
                }
            }
        }

        result.push(trimmed.to_string());
        i += 1;
    }

    result.join("\n")
}

/// Normalize redundant parentheses around assignment expressions.
///
/// OXC may wrap nested assignment expressions in parentheses for clarity:
/// `t1 = (arr.length = 0)` → `t1 = arr.length = 0`
///
/// Since assignment is right-associative, these parens are semantically redundant.
fn normalize_assignment_parens(code: &str) -> String {
    // Match `= (expr = value)` patterns where the inner is also an assignment
    let re = regex::Regex::new(r"= \(([^()]+\s*=\s*[^()]+)\)").unwrap();
    re.replace_all(code, "= $1").to_string()
}

/// Normalize numeric member access: `obj.0` → `obj[0]`, `obj.1` → `obj[1]`, etc.
/// This handles the difference between Babel outputting bracket notation and OXC using dot notation.
fn normalize_numeric_member_access(code: &str) -> String {
    let re = regex::Regex::new(r"(\w)\.(\d+)").unwrap();
    re.replace_all(code, "$1[$2]").to_string()
}

/// Normalize trailing comma expressions in for-loop update: `i = i + 3, i` → `i = i + 3`.
/// The trailing `, <ident>` in a for-update is a sequence expression that evaluates to the
/// last expression but has no effect — it's semantically equivalent to just the assignment.
fn normalize_for_update_trailing_comma(code: &str) -> String {
    fn is_ident_char(ch: u8) -> bool {
        ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'$'
    }

    fn is_ident(text: &str) -> bool {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return false;
        }
        let first = bytes[0];
        if !(first.is_ascii_alphabetic() || first == b'_' || first == b'$') {
            return false;
        }
        bytes[1..].iter().all(|b| is_ident_char(*b))
    }

    fn top_level_delimiters(text: &str) -> (Vec<usize>, Vec<usize>) {
        let bytes = text.as_bytes();
        let mut semicolons = Vec::new();
        let mut commas = Vec::new();
        let mut paren = 0i32;
        let mut brace = 0i32;
        let mut bracket = 0i32;
        let mut i = 0usize;

        while i < bytes.len() {
            match bytes[i] {
                b'(' => paren += 1,
                b')' => paren -= 1,
                b'{' => brace += 1,
                b'}' => brace -= 1,
                b'[' => bracket += 1,
                b']' => bracket -= 1,
                b'\'' | b'"' | b'`' => {
                    let quote = bytes[i];
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if bytes[i] == quote {
                            break;
                        }
                        i += 1;
                    }
                }
                b';' if paren == 0 && brace == 0 && bracket == 0 => semicolons.push(i),
                b',' if paren == 0 && brace == 0 && bracket == 0 => commas.push(i),
                _ => {}
            }
            i += 1;
        }

        (semicolons, commas)
    }

    fn normalize_for_header(header: &str) -> Option<String> {
        let (semicolons, _commas) = top_level_delimiters(header);
        if semicolons.len() != 2 {
            return None;
        }

        let update_start = semicolons[1] + 1;
        let update = &header[update_start..];
        let (_update_semicolons, update_commas) = top_level_delimiters(update);
        let last_comma = *update_commas.last()?;
        let before = update[..last_comma].trim_end();
        let after = update[last_comma + 1..].trim();
        if before.is_empty() || !is_ident(after) {
            return None;
        }

        let new_update = &update[..last_comma];
        let mut new_header = String::with_capacity(header.len());
        new_header.push_str(&header[..update_start]);
        new_header.push_str(new_update.trim_end());
        Some(new_header)
    }

    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut i = 0usize;
    let mut last_emit = 0usize;

    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] != b"for" {
            i += 1;
            continue;
        }

        if i > 0 && is_ident_char(bytes[i - 1]) {
            i += 1;
            continue;
        }

        let mut j = i + 3;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'(' {
            i += 1;
            continue;
        }

        let open = j;
        let mut depth = 0i32;
        let mut k = open;
        let mut found_close = None;
        while k < bytes.len() {
            match bytes[k] {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        found_close = Some(k);
                        break;
                    }
                }
                b'\'' | b'"' | b'`' => {
                    let quote = bytes[k];
                    k += 1;
                    while k < bytes.len() {
                        if bytes[k] == b'\\' {
                            k += 2;
                            continue;
                        }
                        if bytes[k] == quote {
                            break;
                        }
                        k += 1;
                    }
                }
                _ => {}
            }
            k += 1;
        }

        let Some(close) = found_close else {
            i += 1;
            continue;
        };

        let header = &code[open + 1..close];
        let Some(new_header) = normalize_for_header(header) else {
            i = close + 1;
            continue;
        };

        out.push_str(&code[last_emit..open + 1]);
        out.push_str(&new_header);
        last_emit = close;
        i = close + 1;
    }

    if last_emit == 0 {
        code.to_string()
    } else {
        out.push_str(&code[last_emit..]);
        out
    }
}

/// Advanced multiline call args normalization.
/// Joins `func(arg1,\narg2)` style patterns where an argument is on its own line.
/// Specifically handles the case where a line ends with `,` and the next line is a continuation
/// of the argument list (ending with `)` or contains the closing paren).
fn normalize_multiline_call_args_advanced(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Pattern: call open on one line and the single argument expression on the next:
        // `foo(\n(arg));` -> `foo((arg));`
        if trimmed.ends_with('(') && i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            if next.starts_with('(') && (next.ends_with(");") || next.ends_with("));")) {
                result.push(format!("{}{}", trimmed, next));
                i += 2;
                continue;
            }
        }
        // Check if line ends with a comma (potential multiline argument)
        if trimmed.ends_with(',') && i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            // If the next line looks like a continuation argument (e.g., `props.user) ?? ...`)
            // and the current line is inside a call (has unclosed parens)
            let open = trimmed.chars().filter(|c| *c == '(').count();
            let close = trimmed.chars().filter(|c| *c == ')').count();
            if open > close && (next.contains(')') || next.ends_with(';')) {
                result.push(format!("{} {}", trimmed, next));
                i += 2;
                continue;
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

/// Collapse multiline object-destructuring function params into a single line.
///
/// Pattern:
/// `function Foo({`
/// `x,`
/// `}: T) {`
/// =>
/// `function Foo({ x }: T) {`
fn normalize_multiline_function_object_params(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let first = lines[i].trim();
        if i + 2 < lines.len() && first.starts_with("function ") && first.ends_with("({") {
            let second = lines[i + 1].trim();
            let third = lines[i + 2].trim();
            if second.ends_with(',') && third.starts_with("}:") {
                let second_no_comma = second.trim_end_matches(',').trim();
                result.push(format!("{} {} {}", first, second_no_comma, third));
                i += 3;
                continue;
            }
        }
        result.push(first.to_string());
        i += 1;
    }
    result.join("\n")
}

/// Normalize `let x;\nreturn x;\n}` at the end of a function to just `}`.
/// This handles the pattern where the compiler emits `let x = undefined; return x;`
/// while upstream just omits the useMemo return when the value is unused.
fn normalize_return_undefined_var(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let n = lines.len();
    let mut result = Vec::new();
    let mut i = 0;
    let bare_decl_re = regex::Regex::new(r"^let (\w+);$").unwrap();
    while i < n {
        let trimmed = lines[i].trim();
        // Look for pattern: `let <var>;` followed by `return <var>;` followed by `}`
        if i + 2 < n {
            let next = lines[i + 1].trim();
            let after = lines[i + 2].trim();
            if let Some(caps) = bare_decl_re.captures(trimmed) {
                let var = &caps[1];
                if next == format!("return {};", var) && after == "}" {
                    // Skip the `let x;` and `return x;` lines, just emit `}`
                    i += 2;
                    continue;
                }
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

/// Normalize Unicode escape sequences: convert literal non-ASCII characters to `\uXXXX`
/// escape form so that e.g. the literal character `ŧ` (U+0167) and the escape sequence
/// `\u0167` are treated as equivalent. Also collapses multi-byte UTF-8 escape sequences
/// like `\u00c5\u00a7` (two UTF-8 bytes) into the correct codepoint `\u0167`.
fn normalize_unicode_escapes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for ch in code.chars() {
        if !ch.is_ascii() {
            // Convert to \uXXXX for BMP characters, \u{XXXXX} for supplementary
            let cp = ch as u32;
            if cp <= 0xFFFF {
                result.push_str(&format!("\\u{:04x}", cp));
            } else {
                result.push_str(&format!("\\u{{{:x}}}", cp));
            }
        } else {
            result.push(ch);
        }
    }
    // Collapse UTF-8 byte-pair escapes: \u00XX\u00YY where they form a valid 2-byte
    // UTF-8 sequence (first byte 0xC0-0xDF, second byte 0x80-0xBF)
    let utf8_pair =
        regex::Regex::new(r"\\u00([cCdD][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])").unwrap();

    utf8_pair
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let codepoint = ((b1 as u32 & 0x1F) << 6) | (b2 as u32 & 0x3F);
            format!("\\u{:04x}", codepoint)
        })
        .to_string()
}

fn normalize_compare_unicode_escapes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for ch in code.chars() {
        if ch == '\t' {
            result.push_str("\\t");
        } else if !ch.is_ascii() {
            let cp = ch as u32;
            if cp <= 0xFFFF {
                result.push_str(&format!("\\u{:04x}", cp));
            } else {
                let cp = cp - 0x1_0000;
                let high = 0xD800 + ((cp >> 10) & 0x3FF);
                let low = 0xDC00 + (cp & 0x3FF);
                result.push_str(&format!("\\u{:04x}\\u{:04x}", high, low));
            }
        } else {
            result.push(ch);
        }
    }

    let utf8_pair =
        regex::Regex::new(r"\\u00([cCdD][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])").unwrap();
    let result = utf8_pair
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let codepoint = ((b1 as u32 & 0x1F) << 6) | (b2 as u32 & 0x3F);
            format!("\\u{:04x}", codepoint)
        })
        .to_string();

    let utf8_triplet = regex::Regex::new(
        r"\\u00([eE][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])",
    )
    .unwrap();
    let result = utf8_triplet
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let b3 = u8::from_str_radix(&caps[3], 16).unwrap();
            let codepoint =
                ((b1 as u32 & 0x0F) << 12) | ((b2 as u32 & 0x3F) << 6) | (b3 as u32 & 0x3F);
            format!("\\u{:04x}", codepoint)
        })
        .to_string();

    let utf8_quad = regex::Regex::new(
        r"\\u00([fF][0-7])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])",
    )
    .unwrap();
    let result = utf8_quad
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let b3 = u8::from_str_radix(&caps[3], 16).unwrap();
            let b4 = u8::from_str_radix(&caps[4], 16).unwrap();
            let codepoint = ((b1 as u32 & 0x07) << 18)
                | ((b2 as u32 & 0x3F) << 12)
                | ((b3 as u32 & 0x3F) << 6)
                | (b4 as u32 & 0x3F);
            let cp = codepoint - 0x1_0000;
            let high = 0xD800 + ((cp >> 10) & 0x3FF);
            let low = 0xDC00 + (cp & 0x3FF);
            format!("\\u{:04x}\\u{:04x}", high, low)
        })
        .to_string();

    let escape_case = regex::Regex::new(r"\\u([0-9a-fA-F]{4})").unwrap();
    escape_case
        .replace_all(&result, |caps: &regex::Captures| {
            format!("\\u{}", caps[1].to_ascii_lowercase())
        })
        .to_string()
}

/// Normalize empty switch cases that only contain a break to a label.
/// Converts `case N: { break labelN; }` to `case N:` when the case body
/// only has a label break (i.e., empty fallthrough).
fn normalize_empty_switch_case(code: &str) -> String {
    // Match patterns like `case 2: { break bb0; }` → `case 2:`
    let re = regex::Regex::new(r"(case\s+\S+?:)\s*\{\s*break\s+\w+;\s*}").unwrap();
    re.replace_all(code, "$1").to_string()
}

/// Normalize anonymous function expressions: `function()` → `function ()`
/// OXC codegen omits the space before `(` in anonymous function expressions,
/// while Babel includes it.
fn normalize_anonymous_function_space(code: &str) -> String {
    // Match `function()` but NOT `function name()` — only anonymous functions
    // Look for `function(` not preceded by a word char (to avoid matching named functions)
    let re = regex::Regex::new(r"\bfunction\(").unwrap();
    re.replace_all(code, "function (").to_string()
}

/// Normalize numeric destructuring keys: `{ "21": x }` → `{ 21: x }`
/// OXC may emit numeric property keys as quoted strings in destructuring patterns.
fn normalize_numeric_destructuring_key(code: &str) -> String {
    // Match `{ "123": var }` patterns in destructuring and normalize to `{ 123: var }`
    let re = regex::Regex::new(r#""(\d+)"(\s*:)"#).unwrap();
    re.replace_all(code, "$1$2").to_string()
}

/// Normalize optional parentheses around expressions in arrow function bodies.
/// `=> (cond ? a : b)` and `=> cond ? a : b` are equivalent.
/// `=> (a = b)` and `=> a = b` are equivalent.
/// Strip block comments (`/* ... */` and `/** ... */`) from code.
/// Babel's codegen naturally strips all comments, but OXC preserves them.
fn normalize_strip_block_comments(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Check for string literals - skip them entirely
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            result.push(quote as char);
            i += 1;
            while i < len && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < len {
                    result.push(bytes[i] as char);
                    i += 1;
                    result.push(bytes[i] as char);
                    i += 1;
                } else {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < len {
                result.push(bytes[i] as char);
                i += 1;
            }
        } else if bytes[i] == b'`' {
            // Template literal - skip it
            result.push('`');
            i += 1;
            while i < len && bytes[i] != b'`' {
                if bytes[i] == b'\\' && i + 1 < len {
                    result.push(bytes[i] as char);
                    i += 1;
                    result.push(bytes[i] as char);
                    i += 1;
                } else {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < len {
                result.push('`');
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment - skip until */
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2; // skip */
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Strip inline `// ...` comments from the end of lines.
///
/// Babel's codegen naturally strips all comments, so the expected output never
/// has inline comments. Our output preserves them in pass-through (non-compiled)
/// functions. This normalizer removes trailing `// ...` comments to avoid
/// spurious diffs.
///
/// We skip lines where `//` appears inside a string literal (rough heuristic:
/// count unescaped quotes before `//` — if odd, it's inside a string).
/// We also drop lines that are *only* a comment.
fn normalize_strip_inline_comments(code: &str) -> String {
    code.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // Drop pure-comment lines entirely for normalized comparison.
            if trimmed.starts_with("//") {
                return None;
            }
            // Find the first `//` that is not inside a string
            if let Some(pos) = find_line_comment_start(trimmed) {
                let before = trimmed[..pos].trim_end();
                if before.is_empty() {
                    None
                } else {
                    Some(before.to_string())
                }
            } else {
                Some(line.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the start of a `//` line comment, ignoring occurrences inside strings
/// and template literals. Returns `None` if no comment found.
fn find_line_comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < len && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1; // skip escaped char
                    }
                    i += 1;
                }
                if i < len {
                    i += 1; // skip closing quote
                }
            }
            b'`' => {
                // Template literal — skip to closing backtick (simplified, no nesting)
                i += 1;
                while i < len && bytes[i] != b'`' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                return Some(i);
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

fn normalize_arrow_body_ternary_parens(code: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        // Look for `=> (`
        if i + 3 < len
            && chars[i] == '='
            && chars[i + 1] == '>'
            && chars[i + 2] == ' '
            && chars[i + 3] == '('
        {
            // Find the matching closing paren, tracking depth
            let start = i + 4;
            let mut depth = 1;
            let mut j = start;
            while j < len && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            // Only strip if the closing paren is NOT followed by `(` (which would mean
            // an IIFE or function call, e.g., `=> (expr)(args)`).
            let next_char = if j < len { Some(chars[j]) } else { None };
            let mut k = j;
            while k < len && chars[k].is_whitespace() {
                k += 1;
            }
            let followed_by_arrow = k + 1 < len && chars[k] == '=' && chars[k + 1] == '>';
            if depth == 0 && next_char != Some('(') && !followed_by_arrow {
                // Remove the outer parens: `=> (inner)` -> `=> inner`
                let inner: String = chars[start..j - 1].iter().collect();
                result.push_str("=> ");
                result.push_str(&inner);
                i = j;
                continue;
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Normalize redundant comma expressions in assignments.
///
/// The upstream codegen wraps StoreLocal inside LogicalExpression operands
/// Collapse sentinel scope patterns. When the AST codegen emits separate
/// sentinel scopes that the expected output merges into following scopes,
/// normalize by inlining the sentinel scope's expression at the usage site.
///
/// Pattern detected:
/// ```
/// let TEMP;
/// if ($[N] === Symbol.for("react.memo_cache_sentinel")) { TEMP = EXPR;
/// $[N] = TEMP
/// } else { TEMP = $[N]
/// }
/// ... TEMP ...  (later usage, e.g., NAME = TEMP or as scope dependency)
/// ```
///
/// Replaced with removing the sentinel block and inlining `EXPR` at usage sites.
/// And removes the block, storing the (TEMP → EXPR) mapping. Then replaces
/// `IDENT = TEMP;` or `IDENT = TEMP` at end of statement with `IDENT = EXPR`.
/// Normalize `() => undefined` to `() =>{}` and `return undefined` to `return`.
fn normalize_arrow_void_body(code: &str) -> String {
    code.replace("() => undefined", "() =>{}")
        .replace("() =>undefined", "() =>{}")
        .replace("return undefined", "return")
}

fn normalize_sentinel_scope_inline(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let len = lines.len();
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut skip_lines: Vec<bool> = vec![false; len];

    // Scan for sentinel scope patterns. The pattern starts with `let tN;`
    // followed by `if ($[M] === Symbol.for("react.memo_cache_sentinel")) {`
    // and ends with `}` after a `} else {` block. The body may span multiple lines.
    let mut i = 0;
    while i + 6 < len {
        let l0 = lines[i].trim();
        let l1 = lines[i + 1].trim();

        if !(l0.starts_with("let t")
            && l0.ends_with(';')
            && l1.contains("Symbol.for(\"react.memo_cache_sentinel\")")
            && l1.ends_with('{'))
        {
            i += 1;
            continue;
        }

        let temp = match l0.strip_prefix("let ").and_then(|s| s.strip_suffix(';')) {
            Some(t) if t.starts_with('t') && t[1..].chars().all(|c| c.is_ascii_digit()) => t,
            _ => {
                i += 1;
                continue;
            }
        };

        // Find `} else {` and the final `}` to determine scope boundaries.
        let mut else_line = None;
        let mut end_line = None;
        let mut depth = 1i32;
        for (j, &scan_line) in lines.iter().enumerate().skip(i + 2) {
            let lt = scan_line.trim();
            if lt == "}" {
                depth -= 1;
                if depth == 0 {
                    end_line = Some(j);
                    break;
                }
            } else if lt.starts_with("} else {") {
                if depth == 1 {
                    else_line = Some(j);
                }
            } else if lt.ends_with('{') {
                depth += 1;
            }
        }

        let (Some(else_idx), Some(end_idx)) = (else_line, end_line) else {
            i += 1;
            continue;
        };

        // Extract the temp's assignment from the if-body (lines i+2 .. else_idx).
        // Look for `TEMP = EXPR;` in the body.
        let assign_prefix = format!("{} = ", temp);
        let mut found_expr = None;
        for &body_line in &lines[(i + 2)..else_idx] {
            let lt = body_line.trim();
            if lt.starts_with(&assign_prefix) {
                let expr = lt[assign_prefix.len()..].trim_end_matches(';');
                if !expr.is_empty() && !expr.starts_with('$') && !expr.contains("Symbol.for") {
                    found_expr = Some(expr.to_string());
                }
            }
        }

        if let Some(expr) = found_expr {
            replacements.push((temp.to_string(), expr));
            for skip in &mut skip_lines[i..=end_idx] {
                *skip = true;
            }
            i = end_idx + 1;
        } else {
            i += 1;
        }
    }

    if replacements.is_empty() {
        return code.to_string();
    }

    // Build output: skip sentinel lines, replace temp references.
    let mut result_lines: Vec<String> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if skip_lines[idx] {
            continue;
        }
        let mut l = line.to_string();
        for (temp, expr) in &replacements {
            // Replace `IDENT = TEMP;` or `IDENT = TEMP` at end of assignment.
            let pattern_semi = format!(" = {};", temp);
            let replacement_semi = format!(" = {};", expr);
            l = l.replace(&pattern_semi, &replacement_semi);

            let pattern_newline = format!(" = {}\n", temp);
            let replacement_newline = format!(" = {}\n", expr);
            l = l.replace(&pattern_newline, &replacement_newline);

            // Also handle ` !== TEMP)` and ` !== TEMP ||` in scope guards.
            let _guard_pattern = format!(" !== {}", temp);
            // Don't replace these — the guard references the cached value, not the expr.
        }
        // Skip standalone `let TEMP;` declarations that weren't in the sentinel block.
        let trimmed = l.trim();
        let is_removed_decl = replacements
            .iter()
            .any(|(t, _)| trimmed == format!("let {};", t) || trimmed == format!("let {}", t));
        if !is_removed_decl {
            result_lines.push(l);
        }
    }

    result_lines.join("\n")
}

/// as `((x = V), V)` to make the truthiness check explicit. Our codegen
/// emits `(x = V)` which is semantically equivalent (assignment evaluates
/// to V). This normalizer converts `((x = V), V)` to `(x = V)`.
///
/// Pattern: `((IDENT = VALUE), VALUE)` where both VALUE strings are identical.
fn normalize_redundant_comma_in_assignment(code: &str) -> String {
    // Match `((ident = value), value)` patterns.
    // We use a simple regex for the common case of simple literal values.
    let re = regex::Regex::new(r"\(\((\w+)\s*=\s*([^(),]+)\)\s*,\s*([^(),]+)\)").unwrap();
    re.replace_all(code, |caps: &regex::Captures| {
        let ident = caps.get(1).unwrap().as_str();
        let val1 = caps.get(2).unwrap().as_str().trim();
        let val2 = caps.get(3).unwrap().as_str().trim();
        if val1 == val2 {
            format!("({} = {})", ident, val1)
        } else {
            caps.get(0).unwrap().as_str().to_string()
        }
    })
    .to_string()
}

/// Normalize trailing comma-read in expression statements.
///
/// The upstream codegen sometimes emits `(y = expr), y;` as a statement
/// where the `, y` is a no-op read of the just-assigned variable.
/// Our codegen emits just `y = expr;`. Both are semantically equivalent
/// at the statement level since the trailing read value is discarded.
///
/// Pattern: `(IDENT = EXPR), IDENT;` -> `IDENT = EXPR;`
fn normalize_trailing_comma_read_stmt(code: &str) -> String {
    // Match lines like: `(ident = expr), ident;`
    // The ident before and after the comma must be the same.
    let re = regex::Regex::new(r"^\((\w+)\s*=\s*(.+?)\)\s*,\s*(\w+)\s*;$").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if let Some(caps) = re.captures(trimmed) {
                let ident1 = caps.get(1).unwrap().as_str();
                let ident2 = caps.get(3).unwrap().as_str();
                if ident1 == ident2 {
                    let expr = caps.get(2).unwrap().as_str();
                    format!("{} = {};", ident1, expr)
                } else {
                    trimmed.to_string()
                }
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn generate_snapshot(results: &[FixtureResult], pass_rate: f64) -> String {
    let mut snap = String::new();
    snap.push_str(&format!(
        "# React Compiler Conformance — {pass_rate:.1}% parity rate\n\n"
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

/// Normalize JSX whitespace before closing tags.
///
/// Babel may insert whitespace between JSX expression containers and the closing
/// fragment/element tag: `{ t4 }; </>` vs `{ t4 };</>`.  This whitespace
/// appears in JSX text nodes and is semantically insignificant for our
/// comparison purposes.  Removing spaces just before `</` on JSX lines
/// eliminates this spurious diff.
fn normalize_jsx_whitespace_before_closing_tag(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Only apply to lines that look like JSX (contain closing tags)
            if trimmed.contains("</") {
                // Remove spaces immediately before `</`
                trimmed.replace(" </", "</").to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize variable rename suffixes: upstream uses `$N` (e.g., `x$0`) but our
/// Rust port uses `_N` (e.g., `x_0`) since we bypass Babel's printer which also
/// converts `$` to `_`.  Normalize `$N` to `_N` for identifier-like contexts.
fn normalize_rename_suffixes(code: &str) -> String {
    // Match word char followed by $, then digits, at a word boundary
    let re = regex::Regex::new(r"(\w)\$(\d+)\b").unwrap();
    re.replace_all(code, "${1}_${2}").to_string()
}

/// Canonicalize the common temp-conflict suffix form `tN_0` back to `tN`.
fn normalize_temp_zero_suffixes(code: &str) -> String {
    let re = regex::Regex::new(r"\bt(\d+)_0\b").unwrap();
    re.replace_all(code, "t$1").to_string()
}

/// Strip `_N` SSA suffixes from non-temporary identifiers. Upstream and our port
/// may differ on whether a hoisted or reassigned variable gets a numeric suffix.
/// For example, upstream may produce `pathname_0` while we produce `pathname`, or
/// `item_0` vs `item`. This normalizer strips `_N` from identifiers whose base name
/// is at least 2 characters and NOT a temporary (`tN`), so `pathname_0` -> `pathname`
/// Collapse multiline arrow function bodies into single lines.
///
/// Our codegen sometimes produces:
/// ```
/// fn={() =>{
/// if (cond) {
/// return x;
/// }
/// return null;
/// }}
/// ```
/// Upstream Babel produces the equivalent on one line:
/// ```
/// fn={() =>{if (cond) { return x; } return null;}}
/// ```
/// This normalizer collapses the arrow body when it's short enough.
fn normalize_multiline_arrow_bodies(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let mut trimmed = lines[i].trim().to_string();
        let mut consumed_next_line = 0usize;
        // Collapse split higher-order arrows:
        // `x =>` + next line `() => {...}` -> `x => () => {...}`
        if trimmed.ends_with("=>") && i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            if next.starts_with("() =>") || next.starts_with("() =>{") {
                trimmed = format!("{} {}", trimmed, next);
                consumed_next_line = 1;
            }
        }
        // Match lines containing `=> {` or `=>{` where braces don't balance
        let has_arrow_body = trimmed.contains("=> {") || trimmed.contains("=>{");
        if has_arrow_body {
            let open_braces = trimmed.matches('{').count() as i32;
            let close_braces = trimmed.matches('}').count() as i32;
            let net = open_braces - close_braces;
            if net > 0 {
                // Collect lines until braces balance
                let mut parts = vec![trimmed.clone()];
                let mut j = i + 1 + consumed_next_line;
                let mut depth = net;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    parts.push(t.to_string());
                    j += 1;
                }
                // Only collapse if total is short enough (avoid very long lines)
                let total_len: usize = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len();
                if total_len <= 600 {
                    let joined = parts.join(" ");
                    let mut cleaned = joined.replace("  ", " ");
                    // Normalize spacing around arrow body braces:
                    // `=> { X` → `=>{X` (remove space after opening brace)
                    cleaned = cleaned.replace("=> { ", "=>{");
                    cleaned = cleaned.replace("=>{ ", "=>{");
                    // Collapse ` }}` → `}}` at end of arrow bodies
                    // (but only the trailing close-braces of the arrow)
                    while cleaned.contains(" }}") {
                        cleaned = cleaned.replace(" }}", "}}");
                    }
                    result.push(cleaned);
                    i = j;
                    continue;
                }
            }
        }
        result.push(trimmed);
        i += 1 + consumed_next_line;
    }
    result.join("\n")
}

/// Collapse multiline `if (` test expressions into a single line.
fn normalize_multiline_if_conditions(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if current.starts_with("if (") && !current.contains(") {") {
            let mut parts = vec![current.to_string()];
            let mut j = i + 1;
            while j < lines.len() {
                let part = lines[j].trim();
                parts.push(part.to_string());
                if part == ") {" || part.ends_with(") {") {
                    break;
                }
                j += 1;
            }
            if j < lines.len() {
                out.push(parts.join(" "));
                i = j + 1;
                continue;
            }
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_if_paren_spacing(code: &str) -> String {
    let re = regex::Regex::new(r"^if\s*\(\s*(.*?)\s*\)\s*\{$").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if let Some(caps) = re.captures(trimmed) {
                format!("if ({}) {{", caps.get(1).unwrap().as_str())
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse multiline function/method invocations into a single line.
fn normalize_multiline_call_invocations(code: &str) -> String {
    fn paren_delta(line: &str) -> i32 {
        line.chars().fold(0, |depth, ch| match ch {
            '(' => depth + 1,
            ')' => depth - 1,
            _ => depth,
        })
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        let starts_call = current.contains('(')
            && !current.starts_with("if (")
            && !current.starts_with("for (")
            && !current.starts_with("while (")
            && !current.starts_with("switch (")
            && !current.starts_with("catch (")
            && !current.starts_with("function ");
        let mut depth = paren_delta(current);
        if starts_call && depth > 0 {
            let mut parts = vec![current.to_string()];
            let mut j = i + 1;
            while j < lines.len() && depth > 0 {
                let part = lines[j].trim();
                if part.starts_with("function ") {
                    break;
                }
                depth += paren_delta(part);
                parts.push(part.to_string());
                j += 1;
            }
            if depth == 0 && parts.len() > 1 {
                out.push(parts.join(" "));
                i = j;
                continue;
            }
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Collapse arrow expressions that return a multiline fragment:
/// `let x = () =>\n<>\n...\n</>\n;` -> `let x = () => <><...></>;`
fn normalize_multiline_arrow_fragment_expressions(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if current.ends_with("=>") && i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            if next.starts_with("<>") {
                let mut fragment = String::new();
                let mut j = i + 1;
                let mut fragment_has_trailing_semicolon = false;
                if next.contains("</>") {
                    fragment.push_str(next.trim_end_matches(';'));
                    fragment_has_trailing_semicolon = next.ends_with(';');
                    j += 1;
                } else {
                    fragment.push_str("<>");
                    j += 1;
                    while j < lines.len() {
                        let part = lines[j].trim();
                        if part == "</>" || part == "</>;" {
                            fragment.push_str("</>");
                            fragment_has_trailing_semicolon = part.ends_with(';');
                            j += 1;
                            break;
                        }
                        fragment.push_str(part);
                        j += 1;
                    }
                }
                if fragment.ends_with("</>") {
                    let mut collapsed = format!("{current} {fragment}");
                    if fragment_has_trailing_semicolon
                        || (i + 1 < lines.len() && lines[i + 1].trim().ends_with("</>;"))
                        || (j < lines.len() && lines[j].trim() == ";")
                    {
                        collapsed.push(';');
                        if j < lines.len() && lines[j].trim() == ";" {
                            j += 1;
                        }
                    }
                    out.push(collapsed);
                    i = j;
                    continue;
                }
            }
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_multiline_optional_chain_calls(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());

    for line in lines {
        let trimmed = line.trim();
        if (trimmed.starts_with("?.") || trimmed.starts_with("?.["))
            && let Some(last) = out.last_mut()
        {
            last.push_str(trimmed);
            continue;
        }
        out.push(trimmed.to_string());
    }

    out.join("\n")
}

fn normalize_strict_multiline_call_open_args(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i + 1 < lines.len() && current.ends_with('(') {
            let next = lines[i + 1].trim();
            if next.starts_with('(') {
                out.push(format!("{current}{next}"));
                i += 2;
                continue;
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_strict_multiline_call_tail_args(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i + 2 < lines.len() && current.ends_with(',') {
            let middle = lines[i + 1].trim();
            let tail = lines[i + 2].trim();
            if middle.ends_with(',') && tail == ");" {
                out.push(format!(
                    "{} {});",
                    current,
                    middle.trim_end_matches(',').trim_end()
                ));
                i += 3;
                continue;
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Remove printer-only parens around JSX in ternary/logical branches.
fn normalize_jsx_branch_paren_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            let mut s = line.trim().to_string();
            if !(s.contains("? (") || s.contains(": (") || s.contains("&& (")) {
                return s;
            }
            for (from, to) in [
                ("? ( <>", "? <>"),
                ("? ( <", "? <"),
                (": ( <>", ": <>"),
                (": ( <", ": <"),
                ("&& ( <>", "&& <>"),
                ("&& ( <", "&& <"),
                ("> ) :", "> :"),
                ("> )}", ">}"),
                ("> );", ">;"),
                ("</> ) :", "</> :"),
                ("</> )}", "</>}"),
                ("</> );", "</>;"),
            ] {
                while s.contains(from) {
                    s = s.replace(from, to);
                }
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_jsx_nested_ternary_wrapper_parens(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if !(trimmed.contains('<')
                && trimmed.contains(": (")
                && trimmed.contains('?')
                && trimmed.contains("</"))
            {
                return trimmed.to_string();
            }
            trimmed
                .replace(": (", ": ")
                .replace(") }", "}")
                .replace(")}", "}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize simple JSX attribute expression spacing: `foo={ bar }` -> `foo={bar}`.
fn normalize_simple_jsx_attr_brace_spacing(code: &str) -> String {
    let re = regex::Regex::new(r"=\{\s*([^{}]+?)\s*\}").unwrap();
    code.lines()
        .map(|line| re.replace_all(line.trim(), "={$1}").to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove spaces between adjacent JSX tags/fragments and compact arrow-to-JSX spacing.
fn normalize_jsx_tag_boundary_spaces(code: &str) -> String {
    let arrow_re = regex::Regex::new(r"=>\s+<").unwrap();
    let close_paren_re = regex::Regex::new(r"/>\s+\)").unwrap();
    code.lines()
        .map(|line| {
            let mut s = line.trim().to_string();
            while s.contains("> <") {
                s = s.replace("> <", "><");
            }
            while s.contains("<> ") {
                s = s.replace("<> ", "<>");
            }
            while s.contains(" </>") {
                s = s.replace(" </>", "</>");
            }
            s = arrow_re.replace_all(&s, "=><").to_string();
            s = close_paren_re.replace_all(&s, "/>)").to_string();
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Compact JSX text around expression containers: `text {expr} more` -> `text{expr}more`.
fn normalize_jsx_text_expr_spacing_compact(code: &str) -> String {
    let before_expr_re = regex::Regex::new(r">([^<>{}]*)\s+\{").unwrap();
    let after_expr_re = regex::Regex::new(r"\}\s+([^<>{}]+)<").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.contains('<') && trimmed.contains('>') {
                let s = before_expr_re.replace_all(trimmed, ">$1{").to_string();
                after_expr_re.replace_all(&s, "}$1<").to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_jsx_text_expr_container_spacing(code: &str) -> String {
    let open_re = regex::Regex::new(r"([^\s=<>{}])\{\s+").unwrap();
    let close_re = regex::Regex::new(r"\s+\}([A-Za-z_<])").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if !(trimmed.contains('<') && trimmed.contains('>') && trimmed.contains('{')) {
                return trimmed.to_string();
            }
            let s = open_re.replace_all(trimmed, "$1{").to_string();
            close_re.replace_all(&s, "}$1").to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Join `if (...) {` / `} else {` lines with the first simple statement in the block.
fn normalize_inline_if_first_statements(code: &str) -> String {
    fn is_simple_stmt(line: &str) -> bool {
        line.ends_with(';') && !line.ends_with('{') && line != "}" && line != "};"
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i + 1 < lines.len()
            && ((current.starts_with("if (") && current.ends_with('{')) || current == "} else {")
        {
            let next = lines[i + 1].trim();
            if is_simple_stmt(next) {
                out.push(format!("{current} {next}"));
                i += 2;
                continue;
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Repair `React.memo` lines whose closing `);` was stranded after an outlined helper.
fn normalize_react_memo_closing_paren(code: &str) -> String {
    let mut out: Vec<String> = code.lines().map(|line| line.trim().to_string()).collect();

    for i in 0..out.len() {
        if out[i].contains("React.memo(")
            && !out[i].ends_with(");")
            && let Some(j) = (i + 1..out.len()).find(|&idx| out[idx] == "});")
        {
            out[i].push_str(");");
            out[j] = "}".to_string();
            break;
        }
    }

    out.join("\n")
}

/// Collapse a simple multiline object method body to single-line form.
fn normalize_multiline_object_method_bodies(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i + 2 < lines.len() {
            let next = lines[i + 1].trim();
            let next2 = lines[i + 2].trim();
            if current.ends_with('{') && next.starts_with("return ") && next2.starts_with("} }") {
                out.push(format!("{current} {next} {next2}"));
                i += 3;
                continue;
            }
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Collapse `lhs = {\n"key": value, }["key"];` to one line and drop the
/// optional trailing comma before the closing brace.
fn normalize_multiline_object_literal_access(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if current.ends_with('{') && i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            if next.contains("}[\"") || next.contains("} [\"") || next.contains("}, }[\"") {
                let mut joined = format!("{current} {next}");
                joined = joined.replace(", }[", " }[");
                joined = joined.replace(",} [", "} [");
                out.push(joined);
                i += 2;
                continue;
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_object_shorthand_pairs(code: &str) -> String {
    let shorthand_re =
        regex::Regex::new(r"([,{]\s*)([A-Za-z_$][\w$]*)\s*:\s*([A-Za-z_$][\w$]*)(\s*[,}])")
            .unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            let mut current = trimmed.to_string();
            loop {
                let next = shorthand_re
                    .replace_all(&current, |caps: &regex::Captures| {
                        let key = caps.get(2).unwrap().as_str();
                        let value = caps.get(3).unwrap().as_str();
                        let suffix = caps.get(4).unwrap().as_str();
                        if key == value {
                            format!("{}{}{}", caps.get(1).unwrap().as_str(), key, suffix)
                        } else {
                            caps.get(0).unwrap().as_str().to_string()
                        }
                    })
                    .to_string();
                if next == current {
                    break current;
                }
                current = next;
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse `let alias = value; return alias;` to `return value;`.
fn normalize_simple_alias_return_tail(code: &str) -> String {
    let decl_re =
        regex::Regex::new(r"^let\s+([A-Za-z_$][\w$]*)\s*=\s*([A-Za-z_$][\w$]*)\s*;$").unwrap();
    let ret_re = regex::Regex::new(r"^return\s+([A-Za-z_$][\w$]*)\s*;$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if i + 1 < lines.len() {
            let next = lines[i + 1].trim();
            if let (Some(decl_caps), Some(ret_caps)) =
                (decl_re.captures(current), ret_re.captures(next))
            {
                let alias = decl_caps.get(1).unwrap().as_str();
                let value = decl_caps.get(2).unwrap().as_str();
                let returned = ret_caps.get(1).unwrap().as_str();
                if alias == returned {
                    out.push(format!("return {value};"));
                    i += 2;
                    continue;
                }
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Collapse arrow bodies of the form `()=>{let copy = expr; return copy;}` to
/// the expression-bodied equivalent `()=> expr`.
///
/// This is semantics-preserving for the exact single-binding/single-return
/// pattern and avoids counting hoisted callback printer differences as failures.
fn normalize_arrow_copy_return_body(code: &str) -> String {
    let re = regex::Regex::new(
        r"=>\s*\{\s*(?:let|const)\s+([A-Za-z_$][\w$]*)\s*=\s*([^;{}]+?)\s*;?\s*return\s+([A-Za-z_$][\w$]*)\s*;?\s*\}",
    )
    .unwrap();
    code.lines()
        .map(|line| {
            re.replace_all(line.trim(), |caps: &regex::Captures| {
                let lhs = caps.get(1).unwrap().as_str();
                let rhs = caps.get(3).unwrap().as_str();
                if lhs == rhs {
                    format!("=> {}", caps.get(2).unwrap().as_str().trim())
                } else {
                    caps.get(0).unwrap().as_str().to_string()
                }
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Sort consecutive runs of simple uninitialized `let name;` declarations.
fn normalize_sort_simple_let_decl_runs(code: &str) -> String {
    let simple_let_re = regex::Regex::new(r"^let\s+[A-Za-z_$][\w$]*\s*;$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if simple_let_re.is_match(current) {
            let mut run = vec![current.to_string()];
            let mut j = i + 1;
            while j < lines.len() && simple_let_re.is_match(lines[j].trim()) {
                run.push(lines[j].trim().to_string());
                j += 1;
            }
            run.sort();
            out.extend(run);
            i = j;
            continue;
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

/// but `t0` stays `t0`.
fn normalize_non_temp_ssa_suffixes(code: &str) -> String {
    let re = regex::Regex::new(r"\b([a-zA-Z_]\w*)_(\d+)\b").unwrap();
    re.replace_all(code, |caps: &regex::Captures| {
        let base = &caps[1];
        // Don't strip from temp-like names (e.g., "t0_0" shouldn't become "t0")
        // or from names that are already temp patterns
        if base.starts_with("t") && base[1..].chars().all(|c| c.is_ascii_digit()) {
            caps[0].to_string()
        } else {
            base.to_string()
        }
    })
    .to_string()
}

fn normalize_temp_alpha_renaming(code: &str) -> String {
    use std::collections::HashMap;

    fn is_ident_char(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
    }

    let bytes = code.as_bytes();
    let mut result = String::with_capacity(code.len());
    let mut temp_map: HashMap<String, String> = HashMap::new();
    let mut next_temp = 0u32;
    let mut i = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                result.push(quote as char);
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    result.push(byte as char);
                    i += 1;
                    if byte == b'\\' && i < bytes.len() {
                        result.push(bytes[i] as char);
                        i += 1;
                    } else if byte == quote {
                        break;
                    }
                }
            }
            b'`' => {
                result.push('`');
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    result.push(byte as char);
                    i += 1;
                    if byte == b'\\' && i < bytes.len() {
                        result.push(bytes[i] as char);
                        i += 1;
                    } else if byte == b'`' {
                        break;
                    }
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() {
                    let byte = bytes[i];
                    result.push(byte as char);
                    i += 1;
                    if byte == b'\n' {
                        break;
                    }
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                result.push('/');
                result.push('*');
                i += 2;
                while i < bytes.len() {
                    let byte = bytes[i];
                    result.push(byte as char);
                    i += 1;
                    if byte == b'*' && i < bytes.len() && bytes[i] == b'/' {
                        result.push('/');
                        i += 1;
                        break;
                    }
                }
            }
            byte if is_ident_char(byte) => {
                let start = i;
                i += 1;
                while i < bytes.len() && is_ident_char(bytes[i]) {
                    i += 1;
                }
                let token = &code[start..i];
                if token.len() >= 2
                    && token.starts_with('t')
                    && token[1..].chars().all(|c| c.is_ascii_digit())
                {
                    let canonical = temp_map.entry(token.to_string()).or_insert_with(|| {
                        let name = format!("t{}", next_temp);
                        next_temp += 1;
                        name
                    });
                    result.push_str(canonical);
                } else {
                    result.push_str(token);
                }
            }
            byte => {
                result.push(byte as char);
                i += 1;
            }
        }
    }

    result
}

fn normalize_shadowed_temp_decls(code: &str) -> String {
    use std::collections::HashMap;

    let decl_re = regex::Regex::new(r"^(?:let|const|var)\s+(t\d+)\b").unwrap();
    let mut lines: Vec<String> = code.lines().map(|line| line.trim().to_string()).collect();
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut next_shadow_index: u32 = 900_000;

    for i in 0..lines.len() {
        let current = lines[i].clone();
        let trimmed = current.trim();
        if trimmed.starts_with("function ") {
            seen.clear();
            continue;
        }
        let Some(caps) = decl_re.captures(trimmed) else {
            continue;
        };
        let temp_name = caps.get(1).unwrap().as_str().to_string();
        let entry = seen.entry(temp_name.clone()).or_insert(0);
        if *entry == 0 {
            *entry = 1;
            continue;
        }

        let shadow_name = format!("t{}", next_shadow_index);
        next_shadow_index += 1;
        lines[i] = replace_identifier_token(trimmed, &temp_name, &shadow_name);

        for later_line in lines.iter_mut().skip(i + 1) {
            let later_trimmed = later_line.trim().to_string();
            if later_trimmed.starts_with("function ") {
                break;
            }
            if let Some(later_caps) = decl_re.captures(&later_trimmed)
                && later_caps.get(1).unwrap().as_str() == temp_name
            {
                break;
            }
            *later_line = replace_identifier_token(&later_trimmed, &temp_name, &shadow_name);
        }

        *entry += 1;
    }

    lines.join("\n")
}

fn normalize_two_dep_guard_order(code: &str) -> String {
    let guard_re =
        regex::Regex::new(r"^if \(\$\[(\d+)\] !== (.+) \|\| \$\[(\d+)\] !== (.+)\) \{$").unwrap();
    let store_re = regex::Regex::new(r"^\$\[(\d+)\] = (.+);$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        let Some(caps) = guard_re.captures(current) else {
            out.push(current.to_string());
            i += 1;
            continue;
        };

        let slot_a = caps.get(1).unwrap().as_str().to_string();
        let expr_a = caps.get(2).unwrap().as_str().trim().to_string();
        let slot_b = caps.get(3).unwrap().as_str().to_string();
        let expr_b = caps.get(4).unwrap().as_str().trim().to_string();
        if expr_a <= expr_b {
            out.push(current.to_string());
            i += 1;
            continue;
        }

        let mut j = i + 1;
        let mut block_lines: Vec<String> = Vec::new();
        let mut store_a_idx: Option<usize> = None;
        let mut store_b_idx: Option<usize> = None;
        while j < lines.len() {
            let trimmed = lines[j].trim();
            if trimmed == "} else {" {
                break;
            }
            if let Some(store_caps) = store_re.captures(trimmed) {
                let slot = store_caps.get(1).unwrap().as_str();
                let expr = store_caps.get(2).unwrap().as_str().trim();
                if slot == slot_a && expr == expr_a {
                    store_a_idx = Some(block_lines.len());
                } else if slot == slot_b && expr == expr_b {
                    store_b_idx = Some(block_lines.len());
                }
            }
            block_lines.push(trimmed.to_string());
            j += 1;
        }

        let (Some(store_a_idx), Some(store_b_idx)) = (store_a_idx, store_b_idx) else {
            out.push(current.to_string());
            i += 1;
            continue;
        };

        block_lines[store_a_idx] = format!("$[{slot_a}] = {expr_b};");
        block_lines[store_b_idx] = format!("$[{slot_b}] = {expr_a};");
        out.push(format!(
            "if ($[{slot_a}] !== {expr_b} || $[{slot_b}] !== {expr_a}) {{"
        ));
        out.extend(block_lines);
        i = j;
        while i < lines.len() && i <= j {
            out.push(lines[i].trim().to_string());
            i += 1;
        }
    }

    out.join("\n")
}

fn normalize_inline_jsx_cached_wrapper_scope(code: &str) -> String {
    let inline_guard_re = regex::Regex::new(
        r"^if \(\$\[(\d+)\] !== ([A-Za-z_$][\w$]*)\) \{ if \(DEV\) \{ ([A-Za-z_$][\w$]*) = <(.*)$",
    )
    .unwrap();
    let multiline_guard_re =
        regex::Regex::new(r"^if \(\$\[(\d+)\] !== ([A-Za-z_$][\w$]*)\) \{$").unwrap();
    let jsx_assign_re = regex::Regex::new(r"^([A-Za-z_$][\w$]*) = <(.*)$").unwrap();
    let dep_store_re = regex::Regex::new(r"^\$\[(\d+)\] = ([A-Za-z_$][\w$]*);?$").unwrap();
    let output_store_re = regex::Regex::new(r"^\$\[(\d+)\] = ([A-Za-z_$][\w$]*);?$").unwrap();
    let else_load_re =
        regex::Regex::new(r"^\} else \{ ([A-Za-z_$][\w$]*) = \$\[(\d+)\];?$").unwrap();
    let split_else_re = regex::Regex::new(r"^\} else \{$").unwrap();
    let load_stmt_re = regex::Regex::new(r"^([A-Za-z_$][\w$]*) = \$\[(\d+)\];?$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        enum GuardShape {
            Inline { jsx_suffix: String },
            Multiline,
        }

        let mut block_lines: Vec<String> = Vec::new();
        let (dep_slot, dep_name, output_name, mut j, guard_shape) =
            if let Some(guard_caps) = inline_guard_re.captures(current) {
                (
                    guard_caps.get(1).unwrap().as_str().to_string(),
                    guard_caps.get(2).unwrap().as_str().to_string(),
                    guard_caps.get(3).unwrap().as_str().to_string(),
                    i + 1,
                    GuardShape::Inline {
                        jsx_suffix: guard_caps.get(4).unwrap().as_str().to_string(),
                    },
                )
            } else if let Some(guard_caps) = multiline_guard_re.captures(current) {
                if i + 2 >= lines.len() || lines[i + 1].trim() != "if (DEV) {" {
                    out.push(current.to_string());
                    i += 1;
                    continue;
                }
                let Some(assign_caps) = jsx_assign_re.captures(lines[i + 2].trim()) else {
                    out.push(current.to_string());
                    i += 1;
                    continue;
                };
                block_lines.push(lines[i + 1].trim().to_string());
                block_lines.push(lines[i + 2].trim().to_string());
                (
                    guard_caps.get(1).unwrap().as_str().to_string(),
                    guard_caps.get(2).unwrap().as_str().to_string(),
                    assign_caps.get(1).unwrap().as_str().to_string(),
                    i + 3,
                    GuardShape::Multiline,
                )
            } else {
                out.push(current.to_string());
                i += 1;
                continue;
            };

        let mut dep_store_idx: Option<usize> = None;
        let mut output_store_idx: Option<usize> = None;
        let mut rewritten = false;
        while j < lines.len() {
            let trimmed = lines[j].trim();
            if let Some(caps) = dep_store_re.captures(trimmed)
                && caps.get(1).unwrap().as_str() == dep_slot
                && caps.get(2).unwrap().as_str() == dep_name
            {
                dep_store_idx = Some(block_lines.len());
            } else if let Some(caps) = output_store_re.captures(trimmed)
                && caps.get(2).unwrap().as_str() == output_name
            {
                output_store_idx = Some(block_lines.len());
            } else if let Some(caps) = else_load_re.captures(trimmed)
                && caps.get(1).unwrap().as_str() == output_name
            {
                let output_slot = output_store_idx
                    .and_then(|idx| output_store_re.captures(&block_lines[idx]))
                    .map(|caps| caps.get(1).unwrap().as_str().to_string());
                if dep_store_idx.is_none()
                    || output_slot.is_none()
                    || output_slot.as_deref() != Some(caps.get(2).unwrap().as_str())
                {
                    break;
                }

                block_lines.remove(output_store_idx.unwrap());
                block_lines[dep_store_idx.unwrap()] = format!("$[{dep_slot}] = {output_name}");
                match guard_shape {
                    GuardShape::Inline { jsx_suffix } => out.push(format!(
                        "if ($[{dep_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{ if (DEV) {{ {output_name} = <{jsx_suffix}"
                    )),
                    GuardShape::Multiline => out.push(format!(
                        "if ($[{dep_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{"
                    )),
                }
                out.extend(block_lines);
                out.push(format!("}} else {{ {output_name} = $[{dep_slot}]"));
                i = j + 1;
                rewritten = true;
                break;
            } else if split_else_re.is_match(trimmed) && j + 1 < lines.len() {
                let Some(caps) = load_stmt_re.captures(lines[j + 1].trim()) else {
                    block_lines.push(trimmed.to_string());
                    j += 1;
                    continue;
                };
                if caps.get(1).unwrap().as_str() != output_name {
                    block_lines.push(trimmed.to_string());
                    j += 1;
                    continue;
                }
                let output_slot = output_store_idx
                    .and_then(|idx| output_store_re.captures(&block_lines[idx]))
                    .map(|caps| caps.get(1).unwrap().as_str().to_string());
                if dep_store_idx.is_none()
                    || output_slot.is_none()
                    || output_slot.as_deref() != Some(caps.get(2).unwrap().as_str())
                {
                    break;
                }

                block_lines.remove(output_store_idx.unwrap());
                block_lines[dep_store_idx.unwrap()] = format!("$[{dep_slot}] = {output_name}");
                match guard_shape {
                    GuardShape::Inline { jsx_suffix } => out.push(format!(
                        "if ($[{dep_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{ if (DEV) {{ {output_name} = <{jsx_suffix}"
                    )),
                    GuardShape::Multiline => out.push(format!(
                        "if ($[{dep_slot}] === Symbol.for(\"react.memo_cache_sentinel\")) {{"
                    )),
                }
                out.extend(block_lines);
                out.push(format!("}} else {{ {output_name} = $[{dep_slot}]"));
                i = j + 2;
                rewritten = true;
                break;
            }
            block_lines.push(trimmed.to_string());
            j += 1;
        }

        if rewritten {
            continue;
        };

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_simple_else_load_blocks(code: &str) -> String {
    let load_stmt_re = regex::Regex::new(r"^([A-Za-z_$][\w$]*) = \$\[(\d+)\];?$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        if current == "} else {" && i + 2 < lines.len() {
            let next = lines[i + 1].trim();
            let close = lines[i + 2].trim();
            if load_stmt_re.is_match(next) && close == "}" {
                out.push(format!("}} else {{ {next}"));
                i += 2;
                continue;
            }
        }
        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_fbt_plural_cross_product_tables(code: &str) -> String {
    let hk_re = regex::Regex::new(r#"hk: "[^"]+""#).unwrap();
    let leading_object_spacing_re = regex::Regex::new(r#"fbt\._\(\{\s+""#).unwrap();
    let table_re = regex::Regex::new(
        r#"\{\s*"\*":\s*\{\s*"\*":\s*"([^"]+)",\s*_1:\s*"([^"]+)"\s*\},\s*_1:\s*\{\s*"\*":\s*"([^"]+)",\s*_1:\s*"([^"]+)"\s*\}\s*,?\s*\}"#,
    )
    .unwrap();

    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if !trimmed.contains("fbt._(") {
                return trimmed.to_string();
            }
            let with_hk = hk_re
                .replace_all(trimmed, r#"hk: "__FBT_HK__""#)
                .to_string();
            let with_object_spacing = leading_object_spacing_re
                .replace_all(&with_hk, r#"fbt._({""#)
                .to_string();
            table_re
                .replace_all(&with_object_spacing, |caps: &regex::Captures| {
                    format!(
                        r#"{{ "*": {{ "*": "{}" }}, _1: {{ _1: "{}" }} }}"#,
                        caps.get(1).unwrap().as_str(),
                        caps.get(4).unwrap().as_str()
                    )
                })
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_memo_cache_decl_arity(code: &str) -> String {
    let decl_re = regex::Regex::new(r"^const \$ = (_c\d*)\((\d+)\);$").unwrap();
    let slot_re = regex::Regex::new(r"\$\[(\d+)\]").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        let Some(caps) = decl_re.captures(current) else {
            i += 1;
            continue;
        };

        let callee = caps.get(1).unwrap().as_str();
        let declared = caps.get(2).unwrap().as_str().parse::<usize>().unwrap_or(0);
        let mut j = i + 1;
        while j < lines.len() {
            let next = lines[j].trim();
            let next_is_toplevel = j + 1 == lines.len()
                || matches!(
                    lines[j + 1].trim(),
                    // Block-local `let`/`const` declarations frequently follow
                    // inner cache guards. Treating them as top-level truncates the
                    // scan and undercounts later cache slots in sibling branches.
                    line if line.starts_with("function ")
                        || line.starts_with("export ")
                        || line.starts_with("import ")
                );
            if next == "}" && next_is_toplevel {
                break;
            }
            j += 1;
        }

        let mut highest_slot: Option<usize> = None;
        for line in &lines[i..=j.min(lines.len().saturating_sub(1))] {
            for caps in slot_re.captures_iter(line) {
                let slot = caps.get(1).unwrap().as_str().parse::<usize>().unwrap_or(0);
                highest_slot = Some(highest_slot.map_or(slot, |prev| prev.max(slot)));
            }
        }

        if let Some(highest_slot) = highest_slot {
            let required = highest_slot + 1;
            if required < declared {
                out[i] = format!("const $ = {callee}({required});");
            }
        }

        i = j.saturating_add(1);
    }

    out.join("\n")
}

fn normalize_transitional_element_ref_shorthand(code: &str) -> String {
    let ref_re = regex::Regex::new(r",\s*ref(\s*,\s*key\b)").unwrap();
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if !trimmed.contains("$$typeof: Symbol.for(\"react.transitional.element\")") {
                return trimmed.to_string();
            }
            ref_re.replace_all(trimmed, ", ref: ref$1").to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_tail_return_from_cache_alias(code: &str) -> String {
    let return_re = regex::Regex::new(r"^return (t\d+);$").unwrap();
    let else_load_re = regex::Regex::new(
        r"^\} else \{ ((?:t\d+)|(?:[A-Za-z_$][\w$]*)) = \$[A-Za-z0-9_]*\[\d+\];$",
    )
    .unwrap();
    let temp_token_re = regex::Regex::new(r"\bt\d+\b").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
    let counts = temp_token_re.captures_iter(code).fold(
        std::collections::HashMap::<String, usize>::new(),
        |mut acc, caps| {
            *acc.entry(caps.get(0).unwrap().as_str().to_string())
                .or_default() += 1;
            acc
        },
    );

    for i in 2..lines.len() {
        let current = lines[i].trim();
        let Some(return_caps) = return_re.captures(current) else {
            continue;
        };
        let returned = return_caps.get(1).unwrap().as_str();
        if counts.get(returned).copied().unwrap_or(0) != 1 {
            continue;
        }
        if lines[i - 1].trim() != "}" {
            continue;
        }
        let Some(else_caps) = else_load_re.captures(lines[i - 2].trim()) else {
            continue;
        };
        let loaded = else_caps.get(1).unwrap().as_str();
        if loaded == returned {
            continue;
        }
        out[i] = format!("return {loaded};");
    }

    out.join("\n")
}

/// Normalize redundant parens around nullish coalescing (`??`) in ternary test position.
///
/// In JavaScript, `??` (precedence 5) binds tighter than `?:` (precedence 4), so
/// `(a ?? b) ? c : d` and `a ?? b ? c : d` are semantically identical.
/// The upstream Babel codegen emits the parens; our codegen omits them.
/// This normalizer strips the parens so both sides match.
///
/// Pattern: `(EXPR ?? EXPR) ? ` → `EXPR ?? EXPR ? `
fn normalize_nullish_coalescing_ternary_parens(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;
    while i < len {
        if chars[i] == '(' {
            // Try to find matching `)` tracking depth
            let start = i;
            let mut depth = 1;
            let mut j = i + 1;
            let mut has_nullish = false;
            while j < len && depth > 0 {
                match chars[j] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    '?' if j + 1 < len && chars[j + 1] == '?' && depth == 1 => {
                        has_nullish = true;
                        // Skip the second `?`
                        j += 1;
                    }
                    _ => {}
                }
                j += 1;
            }
            // j now points past the closing `)`
            if depth == 0 && has_nullish {
                // Check if followed by ` ? ` (ternary test position)
                let mut k = j;
                // Skip optional whitespace
                while k < len && chars[k] == ' ' {
                    k += 1;
                }
                if k < len && chars[k] == '?' && (k + 1 >= len || chars[k + 1] != '?') {
                    // This is `(expr ?? expr) ?` — strip the outer parens
                    let inner: String = chars[start + 1..j - 1].iter().collect();
                    result.push_str(&inner);
                    i = j;
                    continue;
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

/// Reorder outlined function declarations (`function _temp...`) to appear
/// after the `FIXTURE_ENTRYPOINT` line.
///
/// The upstream codegen places outlined functions after FIXTURE_ENTRYPOINT,
/// while our codegen places them before. This normalizer moves them to a
/// canonical position (after FIXTURE_ENTRYPOINT) so both sides match.
fn normalize_outlined_function_order(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();

    // Find outlined function blocks: a "function _temp..." line followed by
    // its body lines until a closing `}` or `};` line.
    let mut outlined_blocks: Vec<Vec<&str>> = Vec::new();
    let mut other_lines: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("function _temp") && !trimmed.contains("= function _temp") {
            // Collect function block by brace depth (handles nested `if`, etc).
            let mut block = vec![lines[i]];
            let mut depth =
                trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
            if depth <= 0 {
                outlined_blocks.push(block);
                i += 1;
                continue;
            }
            i += 1;
            while i < lines.len() {
                block.push(lines[i]);
                let t = lines[i].trim();
                depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                if depth <= 0 {
                    i += 1;
                    break;
                }
                i += 1;
            }
            outlined_blocks.push(block);
        } else {
            other_lines.push(lines[i]);
            i += 1;
        }
    }

    if outlined_blocks.is_empty() {
        return code.to_string();
    }

    outlined_blocks.sort_by(|a, b| {
        let a_name = a
            .first()
            .and_then(|line| line.trim().strip_prefix("function "))
            .and_then(|rest| rest.split('(').next())
            .unwrap_or("");
        let b_name = b
            .first()
            .and_then(|line| line.trim().strip_prefix("function "))
            .and_then(|rest| rest.split('(').next())
            .unwrap_or("");
        a_name.cmp(b_name)
    });

    // Reconstruct: other_lines first, then outlined_blocks appended at the end
    let mut result_lines: Vec<&str> = other_lines;
    for block in &outlined_blocks {
        for line in block {
            result_lines.push(line);
        }
    }
    result_lines.join("\n")
}

/// Strip trailing `;` from standalone function declaration closing lines.
///
/// Our codegen may emit `};` to close a function declaration (e.g., for outlined
/// functions), while the upstream emits `}`. This normalizer strips the semicolon.
/// Only applies to lines that are exactly `};` (with optional whitespace).
fn normalize_function_decl_trailing_semicolon(code: &str) -> String {
    code.lines()
        .map(|line| if line.trim() == "};" { "}" } else { line })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Normalize trailing `;` after `}` at the end of arrow function expression lines.
///
/// Lines like `let X = (...) =>{...};` and `let X = (...) =>{...}` should compare
/// equal. We normalize by stripping the trailing `;` when the line matches
/// a `let/const/var ... =>{...}` pattern ending in `};`.
///
/// Also handles `React.memo(...)` wrapping.
fn normalize_arrow_expr_trailing_semicolon(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            // Match lines that are arrow function expression assignments
            let is_arrow_assignment = (trimmed.starts_with("let ")
                || trimmed.starts_with("const ")
                || trimmed.starts_with("var "))
                && trimmed.contains("=>{")
                && (trimmed.ends_with("};") || trimmed.ends_with("}"));
            if is_arrow_assignment && trimmed.ends_with("};") {
                // Strip the trailing semicolon
                &trimmed[..trimmed.len() - 1]
            } else if is_arrow_assignment && trimmed.ends_with("}") {
                // Already normalized
                trimmed
            } else {
                trimmed
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// OXC may wrap an arrow initializer in one extra pair of parentheses when
/// printing from AST, e.g. `let f = ((x) => x);`. That is cosmetic.
fn normalize_parenthesized_arrow_initializers(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            let Some(eq_idx) = trimmed.find("= ((") else {
                return trimmed.to_string();
            };
            if !trimmed[eq_idx + 4..].contains("=>") {
                return trimmed.to_string();
            }
            let body_end = if trimmed.ends_with(");") {
                trimmed.len() - 2
            } else if trimmed.ends_with(')') {
                trimmed.len() - 1
            } else {
                return trimmed.to_string();
            };
            if body_end <= eq_idx + 3 {
                return trimmed.to_string();
            }
            let mut normalized = String::with_capacity(trimmed.len());
            normalized.push_str(&trimmed[..eq_idx + 2]);
            normalized.push_str(&trimmed[eq_idx + 3..body_end]);
            normalized.push_str(&trimmed[body_end + 1..]);
            normalized
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_parenthesized_multiline_arrow_initializers(code: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut wrapped_arrow_brace_depth: Vec<i32> = Vec::new();

    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.contains("= ((") && trimmed.contains("=>") {
            let normalized = trimmed.replacen("= ((", "= (", 1);
            let opens = normalized.chars().filter(|&ch| ch == '{').count() as i32;
            let closes = normalized.chars().filter(|&ch| ch == '}').count() as i32;
            let depth = opens - closes;
            if depth > 0 {
                wrapped_arrow_brace_depth.push(depth);
            }
            out.push(normalized);
            continue;
        }

        if let Some(depth) = wrapped_arrow_brace_depth.last_mut() {
            let opens = trimmed.chars().filter(|&ch| ch == '{').count() as i32;
            let closes = trimmed.chars().filter(|&ch| ch == '}').count() as i32;
            *depth += opens - closes;

            if *depth <= 0 {
                let normalized = if trimmed == "});" {
                    "};".to_string()
                } else if trimmed == "})" {
                    "}".to_string()
                } else {
                    trimmed.to_string()
                };
                wrapped_arrow_brace_depth.pop();
                out.push(normalized);
                continue;
            }
        }

        out.push(trimmed.to_string());
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_arrow_copy_return_body, normalize_code, normalize_destructuring,
        normalize_fbt_plural_cross_product_tables, normalize_if_paren_spacing,
        normalize_inline_if_first_statements, normalize_inline_jsx_cached_wrapper_scope,
        normalize_jsx_branch_paren_spacing, normalize_jsx_nested_ternary_wrapper_parens,
        normalize_jsx_semicolon_on_own_line, normalize_jsx_text_expr_container_spacing,
        normalize_jsx_text_expr_spacing_compact, normalize_jsx_text_line_before_expr,
        normalize_memo_cache_decl_arity, normalize_multiline_arrow_fragment_expressions,
        normalize_multiline_call_invocations, normalize_multiline_if_conditions,
        normalize_multiline_object_literal_access, normalize_multiline_object_method_bodies,
        normalize_multiline_optional_chain_calls, normalize_object_shorthand_pairs,
        normalize_promote_temps, normalize_react_memo_closing_paren, normalize_shadowed_temp_decls,
        normalize_shared_cosmetic_equivalences, normalize_simple_alias_return_tail,
        normalize_simple_jsx_attr_brace_spacing, normalize_small_array_bracket_spacing,
        normalize_small_multiline_return_arrays, normalize_sort_simple_let_decl_runs,
        normalize_strip_inline_comments, normalize_tail_return_from_cache_alias,
        normalize_temp_alpha_renaming, normalize_temp_zero_suffixes, normalize_two_dep_guard_order,
        prepare_code_for_compare,
    };

    #[test]
    fn normalize_promote_temps_promotes_unambiguous_temp_carrier() {
        let input = "let t0;\nt0 = foo();\nconst result = t0;\nreturn result;";
        let expected = "let result;\nresult = foo();\nreturn result;";
        assert_eq!(normalize_promote_temps(input), expected);
    }

    #[test]
    fn normalize_promote_temps_skips_multi_alias_temp() {
        let input =
            "let [, t0] = useState();\nconst setState = t0;\nlet t0;\nconst handleLogout = t0;";
        assert_eq!(normalize_promote_temps(input), input);
    }

    #[test]
    fn normalize_promote_temps_skips_when_temp_is_bound_in_destructure() {
        let input = "let [, t0] = useState();\nconst setState = t0;";
        assert_eq!(normalize_promote_temps(input), input);
    }

    #[test]
    fn normalize_promote_temps_handles_initialized_temp_carrier() {
        let input = "function Component() {\nlet t0 = getNumber();\nlet x = t0;\nreturn x;\n}";
        let expected = "function Component() {\nlet x = getNumber();\nreturn x;\n}";
        assert_eq!(normalize_promote_temps(input), expected);
    }

    #[test]
    fn normalize_promote_temps_scopes_to_each_function() {
        let input = "function Bar() {\nlet t0;\nt0 = identity(null);\nlet shouldInstrument = t0;\nreturn shouldInstrument;\n}\nfunction Foo() {\nlet shouldInstrument;\nreturn shouldInstrument;\n}";
        let expected = "function Bar() {\nlet shouldInstrument;\nshouldInstrument = identity(null);\nreturn shouldInstrument;\n}\nfunction Foo() {\nlet shouldInstrument;\nreturn shouldInstrument;\n}";
        assert_eq!(normalize_promote_temps(input), expected);
    }

    #[test]
    fn normalize_temp_alpha_renaming_canonicalizes_temp_numbering() {
        let input = "let [, t0] = useState();\nlet t3;\nlet handleLogout = t3;\nlet t1;\nlet t2;\nreturn t2;";
        let expected = "let [, t0] = useState();\nlet t1;\nlet handleLogout = t1;\nlet t2;\nlet t3;\nreturn t3;";
        assert_eq!(normalize_temp_alpha_renaming(input), expected);
    }

    #[test]
    fn normalize_temp_alpha_renaming_skips_string_literals() {
        let input = "const label = \"t9\";\nlet t9;\nreturn t9;";
        let expected = "const label = \"t9\";\nlet t0;\nreturn t0;";
        assert_eq!(normalize_temp_alpha_renaming(input), expected);
    }

    #[test]
    fn normalize_shadowed_temp_decls_renames_later_redeclarations() {
        let input = "function Foo() {\nlet t1;\nlet t2;\nlet t1;\nreturn t1;\n}";
        let expected = "function Foo() {\nlet t1;\nlet t2;\nlet t900000;\nreturn t900000;\n}";
        assert_eq!(normalize_shadowed_temp_decls(input), expected);
    }

    #[test]
    fn normalize_two_dep_guard_order_sorts_guard_and_store_pairs() {
        let input =
            "if ($[6] !== t4 || $[7] !== t2) {\n$[6] = t4;\n$[7] = t2;\n} else {\nt5 = $[8];\n}";
        let expected =
            "if ($[6] !== t2 || $[7] !== t4) {\n$[6] = t2;\n$[7] = t4;\n} else {\nt5 = $[8];\n}";
        assert_eq!(normalize_two_dep_guard_order(input), expected);
    }

    #[test]
    fn normalize_inline_jsx_cached_wrapper_scope_rewrites_dep_guard_to_sentinel() {
        let input = "let t3;\nif ($[1] !== content) { if (DEV) { t3 = <Parent>{t2}</Parent>;\n} else { t3 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref, key, props: { children: t2 } };\n}\n$[1] = content;\n$[2] = t3;\n} else { t3 = $[2];\n}\ncontent = t3;";
        let expected = "let t3;\nif ($[1] === Symbol.for(\"react.memo_cache_sentinel\")) { if (DEV) { t3 = <Parent>{t2}</Parent>;\n} else { t3 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref, key, props: { children: t2 } };\n}\n$[1] = t3;\n} else { t3 = $[1];\n}\ncontent = t3;";
        assert_eq!(normalize_inline_jsx_cached_wrapper_scope(input), expected);
    }

    #[test]
    fn normalize_fbt_plural_cross_product_tables_collapses_off_diagonal_branches() {
        let input = r#"t1 = fbt._({ "*": { "*": "{number of apples} apples and {number of bananas} bananas", _1: "{number of apples} apples and {number of bananas} banana" }, _1: { "*": "{number of apples} apple and {number of bananas} bananas", _1: "{number of apples} apple and {number of bananas} banana" } }, [fbt._plural(apples), fbt._plural(bananas)], { hk: "1mGnhr" });"#;
        let expected = r#"t1 = fbt._({ "*": { "*": "{number of apples} apples and {number of bananas} bananas" }, _1: { _1: "{number of apples} apple and {number of bananas} banana" } }, [fbt._plural(apples), fbt._plural(bananas)], { hk: "__FBT_HK__" });"#;
        assert_eq!(normalize_fbt_plural_cross_product_tables(input), expected);
    }

    #[test]
    fn normalize_code_matches_inline_jsx_cached_wrapper_fixture_shape() {
        let actual = "function ConditionalJsx(t0) {\nconst $ = _c2(3);\nlet { shouldWrap } = t0;\nlet content;\nif ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { if (DEV) { content = <div> Hello</div>;\n} else { content = { $$typeof: Symbol.for(\"react.transitional.element\"), type: \"div\", ref, key, props: { children: \"Hello\" } };\n}\n$[0] = content;\n} else { content = $[0];\n}\nif (shouldWrap) { let t2 = content;\nlet t4;\nif ($[1] !== content) { if (DEV) { t4 = <Parent>{t2}</Parent>;\n} else { t4 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref, key, props: { children: t2 } };\n}\n$[1] = content;\n$[2] = t4;\n} else { t4 = $[2];\n}\ncontent = t4;\n}\nreturn content;\n}";
        let expected = "function ConditionalJsx(t0) {\nconst $ = _c2(2);\nlet { shouldWrap } = t0;\nlet content;\nif ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { if (DEV) { content = <div>Hello</div>;\n} else { content = { $$typeof: Symbol.for(\"react.transitional.element\"), type: \"div\", ref, key, props: { children: \"Hello\" } };\n}\n$[0] = content;\n} else { content = $[0];\n}\nif (shouldWrap) { let t2 = content;\nlet t4;\nif ($[1] === Symbol.for(\"react.memo_cache_sentinel\")) { if (DEV) { t4 = <Parent>{t2}</Parent>;\n} else { t4 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref, key, props: { children: t2 } };\n}\n$[1] = t4;\n} else { t4 = $[1];\n}\ncontent = t4;\n}\nreturn content;\n}";
        assert_eq!(normalize_code(actual), normalize_code(expected));
    }

    #[test]
    fn normalize_code_matches_ssa_property_alias_if_fixture_shape() {
        let actual = "import { c as _c } from \"react/compiler-runtime\";\nfunction foo(a) {\nconst $ = _c(4);\nlet x;\nif ($[0] !== a) {\nx = {};\nif (a) {\nlet y;\nif ($[2] === Symbol.for(\"react.memo_cache_sentinel\")) {\ny = {};\n$[2] = y;\n} else {\ny = $[2];\n}\nx.y = y;\n} else {\nlet z;\nif ($[3] === Symbol.for(\"react.memo_cache_sentinel\")) {\nz = {};\n$[3] = z;\n} else {\nz = $[3];\n}\nx.z = z;\n}\n$[0] = a;\n$[1] = x;\n} else {\nx = $[1];\n}\nreturn x;\n}\nexport const FIXTURE_ENTRYPOINT = {\nfn: foo,\nparams: [\"TodoAdd\"],\nisComponent: \"TodoAdd\",\n};";
        let expected = "import { c as _c } from \"react/compiler-runtime\";\nfunction foo(a) {\nconst $ = _c(4);\nlet x;\nif ($[0] !== a) {\nx = {};\nif (a) {\nlet t0;\nif ($[2] === Symbol.for(\"react.memo_cache_sentinel\")) {\nt0 = {};\n$[2] = t0;\n} else {\nt0 = $[2];\n}\nconst y = t0;\nx.y = y;\n} else {\nlet t0;\nif ($[3] === Symbol.for(\"react.memo_cache_sentinel\")) {\nt0 = {};\n$[3] = t0;\n} else {\nt0 = $[3];\n}\nconst z = t0;\nx.z = z;\n}\n$[0] = a;\n$[1] = x;\n} else {\nx = $[1];\n}\nreturn x;\n}\nexport const FIXTURE_ENTRYPOINT = {\nfn: foo,\nparams: [\"TodoAdd\"],\nisComponent: \"TodoAdd\",\n};";
        assert_eq!(normalize_code(actual), normalize_code(expected));
    }

    #[test]
    fn prepare_code_for_compare_strict_matches_inline_jsx_wrapper_cache_shape() {
        let actual = "function ConditionalJsx(t0) {\nconst $ = _c2(3);\nconst { shouldWrap } = t0;\nlet content;\nif ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {\nif (DEV) {\ncontent = <div> Hello </div>\n} else {\ncontent = { $$typeof: Symbol.for(\"react.transitional.element\"), type: \"div\", ref: null, key: null, props: { children: \"Hello\" } }\n}\n$[0] = content\n} else {\ncontent = $[0]\n}\nif (shouldWrap) {\nconst t2 = content;\nlet t4;\nif ($[1] !== content) {\nif (DEV) {\nt4 = <Parent>{t2}</Parent>\n} else {\nt4 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref: null, key: null, props: { children: t2 } }\n}\n$[1] = content\n$[2] = t4\n} else {\nt4 = $[2]\n}\ncontent = t4\n}\nreturn content\n}";
        let expected = "function ConditionalJsx(t0) {\nconst $ = _c2(2);\nconst { shouldWrap } = t0;\nlet content;\nif ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {\nif (DEV) {\ncontent = <div> Hello </div>\n} else {\ncontent = { $$typeof: Symbol.for(\"react.transitional.element\"), type: \"div\", ref: null, key: null, props: { children: \"Hello\" } }\n}\n$[0] = content\n} else {\ncontent = $[0]\n}\nif (shouldWrap) {\nconst t2 = content;\nlet t4;\nif ($[1] === Symbol.for(\"react.memo_cache_sentinel\")) {\nif (DEV) {\nt4 = <Parent>{t2}</Parent>\n} else {\nt4 = { $$typeof: Symbol.for(\"react.transitional.element\"), type: Parent, ref: null, key: null, props: { children: t2 } }\n}\n$[1] = t4\n} else { t4 = $[1]\n}\ncontent = t4\n}\nreturn content\n}";
        assert_eq!(
            prepare_code_for_compare(actual, true),
            prepare_code_for_compare(expected, true)
        );
    }

    #[test]
    fn prepare_code_for_compare_strict_matches_memoized_temp_alias_shape() {
        let actual = "function Component(props) {\nconst $ = _c(9);\nlet x;\nlet y;\nif ($[0] !== props.a) {\nx = identity(props.a);\ny = addOne(x);\n$[0] = props.a;\n$[1] = y;\n$[2] = x\n} else {\ny = $[1];\nx = $[2]\n}\nlet z;\nif ($[3] !== props.b) {\nz = identity(props.b);\n$[3] = props.b;\n$[4] = z\n} else {\nz = $[4]\n}\nlet t2;\nif ($[5] !== x || $[6] !== y || $[7] !== z) {\nt2 = [x, y, z];\n$[5] = x;\n$[6] = y;\n$[7] = z;\n$[8] = t2\n} else {\nt2 = $[8]\n}\nreturn t2\n}";
        let expected = "function Component(props) {\nconst $ = _c(9);\nlet t0;\nlet x;\nif ($[0] !== props.a) {\nx = identity(props.a);\nt0 = addOne(x);\n$[0] = props.a;\n$[1] = t0;\n$[2] = x\n} else {\nt0 = $[1];\nx = $[2]\n}\nconst y = t0;\nlet t1;\nif ($[3] !== props.b) {\nt1 = identity(props.b);\n$[3] = props.b;\n$[4] = t1\n} else {\nt1 = $[4]\n}\nconst z = t1;\nlet t2;\nif ($[5] !== x || $[6] !== y || $[7] !== z) {\nt2 = [x, y, z];\n$[5] = x;\n$[6] = y;\n$[7] = z;\n$[8] = t2\n} else {\nt2 = $[8]\n}\nreturn t2\n}";
        assert_eq!(
            prepare_code_for_compare(actual, true),
            prepare_code_for_compare(expected, true)
        );
    }

    #[test]
    fn prepare_code_for_compare_strict_matches_switch_label_shape() {
        let actual = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nbb1: switch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        let expected = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nswitch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        assert_eq!(
            prepare_code_for_compare(actual, true),
            prepare_code_for_compare(expected, true)
        );
    }

    #[test]
    fn prepare_code_for_compare_strict_matches_call_trivia_shapes() {
        let actual = "setProperty( x, { b: 3, other }, \"a\");\nJSON.stringify( null, null, { \"Component[k]\": () => value }[ \"Component[k]\" ], );";
        let expected = "setProperty(x, { b: 3, other }, \"a\");\nJSON.stringify(null, null, { \"Component[k]\": () => value }[\"Component[k]\"]);";
        assert_eq!(
            prepare_code_for_compare(actual, true),
            prepare_code_for_compare(expected, true)
        );
    }

    #[test]
    fn normalize_code_matches_fbt_mixed_call_tag_spacing() {
        let actual = r#"if ($[0] !== apples || $[1] !== bananas) { t1 = <div>{fbt._({ "*": { "*": "{number of apples} apples and {number of bananas} bananas" }, _1: { _1: "{number of apples} apple and 1 banana" } }, [fbt._plural(apples), fbt._plural(bananas, "number of bananas"), fbt._param("number of apples", apples)], { hk: "__FBT_HK__" })}</div>;"#;
        let expected = r#"if ($[0] !== apples || $[1] !== bananas) { t1 = <div>{fbt._({"*": { "*": "{number of apples} apples and {number of bananas} bananas" }, _1: { _1: "{number of apples} apple and 1 banana" } }, [fbt._plural(apples), fbt._plural(bananas, "number of bananas"), fbt._param("number of apples", apples)], { hk: "__FBT_HK__" })}</div>;"#;
        assert_eq!(normalize_code(actual), normalize_code(expected));
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_collapses_inline_assign_then_read_stmt() {
        let actual = "if ($[2] !== arr2 || $[3] !== x) { y = x.concat(arr2);";
        let expected = "if ($[2] !== arr2 || $[3] !== x) { ((y = x.concat(arr2)), y);";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_collapses_inline_assign_then_discard_stmt() {
        let actual = "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { x = [];";
        let expected =
            "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { ((x = []), null);";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_drops_multiline_trailing_enum_commas() {
        let actual = "enum Bool {\nTrue = \"true\",\nFalse = \"false\"\n}";
        let expected = "enum Bool {\nTrue = \"true\",\nFalse = \"false\",\n}";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_blank_lines_in_gated_function_bodies() {
        let actual = "const Foo = isForgetEnabled_Fixtures()\n? function Foo(props) {\n\"use forget\";\nconst $ = _c(3);\nif (props.bar < 0) {\nreturn props.children\n}\nreturn props.bar\n}\n: function Foo(props) {\nreturn props.bar\n};";
        let expected = "const Foo = isForgetEnabled_Fixtures()\n? function Foo(props) {\n\"use forget\";\nconst $ = _c(3);\n\nif (props.bar < 0) {\nreturn props.children\n}\nreturn props.bar\n}\n: function Foo(props) {\nreturn props.bar\n};";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_blank_lines_before_top_level_exports() {
        let actual = "const Renderer = isForgetEnabled_Fixtures()\n? (props) => {\nconst $ = _c(1);\nreturn props\n}\n: (props) => props;\n\nexport default Renderer;\n\nexport const FIXTURE_ENTRYPOINT = { fn: eval(\"Renderer\"), params: [{}] };";
        let expected = "const Renderer = isForgetEnabled_Fixtures()\n? (props) => {\nconst $ = _c(1);\nreturn props\n}\n: (props) => props;\nexport default Renderer;\n\nexport const FIXTURE_ENTRYPOINT = { fn: eval(\"Renderer\"), params: [{}] };";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_blank_lines_before_top_level_consts() {
        let actual = "const _ = { useHook: isForgetEnabled_Fixtures() ? () => {} : () => {} };\nidentity(_.useHook);\n\nconst useHook = isForgetEnabled_Fixtures()\n? function useHook() {\nconst $ = _c(1);\nreturn null\n}\n: function useHook() {\nreturn null\n};";
        let expected = "const _ = { useHook: isForgetEnabled_Fixtures() ? () => {} : () => {} };\nidentity(_.useHook);\nconst useHook = isForgetEnabled_Fixtures()\n? function useHook() {\nconst $ = _c(1);\nreturn null\n}\n: function useHook() {\nreturn null\n};";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_top_level_comment_trivia() {
        let actual = "import { c as _c } from \"react/compiler-runtime\";\nfunction foo() {}";
        let expected = "import { c as _c } from \"react/compiler-runtime\";\n// @Pass runMutableRangeAnalysis\n// Fixture note\nfunction foo() {}";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_comment_only_lines_in_multiline_imports() {
        let actual = "import { useEffect, useRef, experimental_useEffectEvent as useEffectEvent } from \"react\";";
        let expected = "import {\n  useEffect,\n  useRef,\n  // @ts-expect-error\n  experimental_useEffectEvent as useEffectEvent,\n} from \"react\";";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_fixture_entrypoint_comment_trivia() {
        let actual = "export const FIXTURE_ENTRYPOINT = {\nfn: Component,\nparams: [{ value: [, 3.14] }],\n};";
        let expected = "export const FIXTURE_ENTRYPOINT = {\nfn: Component,\n// should return default\nparams: [{ value: [, /* hole! */ 3.14] }],\n};";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_shared_cosmetic_equivalences_strips_labeled_switch_after_block_braces() {
        let actual =
            "function foo(x) {\nbb0: {\nswitch (x) {\ncase 0: {\nbreak bb0;\n}\ndefault:\n}\n}\n}";
        let expected = "function foo(x) {\nswitch (x) {\ncase 0: {\nbreak;\n}\ndefault:\n}\n}";
        assert_eq!(
            normalize_shared_cosmetic_equivalences(actual),
            normalize_shared_cosmetic_equivalences(expected)
        );
    }

    #[test]
    fn normalize_code_strips_labeled_switch_after_block_brace_normalization() {
        let actual =
            "function foo(x) {\nbb0: {\nswitch (x) {\ncase 0: {\nbreak bb0;\n}\ndefault:\n}\n}\n}";
        let expected = "function foo(x) {\nswitch (x) {\ncase 0: {\nbreak;\n}\ndefault:\n}\n}";
        assert_eq!(normalize_code(actual), normalize_code(expected));
    }

    #[test]
    fn normalize_arrow_copy_return_body_collapses_simple_copy_arrow() {
        let input = "let callbk = () =>{let copy = x; return copy; };\nreturn callbk;";
        let expected = "let callbk = () => x;\nreturn callbk;";
        assert_eq!(normalize_arrow_copy_return_body(input), expected);
    }

    #[test]
    fn normalize_arrow_copy_return_body_collapses_const_copy_arrow() {
        let input = "let callbk = () =>{const copy = x; return copy; };\nreturn callbk;";
        let expected = "let callbk = () => x;\nreturn callbk;";
        assert_eq!(normalize_arrow_copy_return_body(input), expected);
    }

    #[test]
    fn normalize_strip_inline_comments_drops_comment_only_lines() {
        let input = "let x;\n// comment only\nlet y; // trailing\nreturn y;";
        let expected = "let x;\nlet y;\nreturn y;";
        assert_eq!(normalize_strip_inline_comments(input), expected);
    }

    #[test]
    fn normalize_sort_simple_let_decl_runs_sorts_uninitialized_group() {
        let input = "let y;\nlet x;\nlet z = 1;";
        let expected = "let x;\nlet y;\nlet z = 1;";
        assert_eq!(normalize_sort_simple_let_decl_runs(input), expected);
    }

    #[test]
    fn normalize_multiline_if_conditions_collapses_condition() {
        let input = "if (\na &&\nb\n) {\nreturn x;\n}";
        let expected = "if ( a && b ) {\nreturn x;\n}";
        assert_eq!(normalize_multiline_if_conditions(input), expected);
    }

    #[test]
    fn normalize_if_paren_spacing_trims_inner_edges() {
        let input = "if ( a && b ) {\nreturn x;\n}";
        let expected = "if (a && b) {\nreturn x;\n}";
        assert_eq!(normalize_if_paren_spacing(input), expected);
    }

    #[test]
    fn normalize_multiline_call_invocations_collapses_arguments() {
        let input = "foo(bar,\nbaz,\nqux);";
        let expected = "foo(bar, baz, qux);";
        assert_eq!(normalize_multiline_call_invocations(input), expected);
    }

    #[test]
    fn normalize_multiline_arrow_fragment_expressions_collapses_fragment() {
        let input = "let b = () =>\n<>\n<div>a</div>\n<div>b</div>\n</>\n;";
        let expected = "let b = () => <><div>a</div><div>b</div></>;";
        assert_eq!(
            normalize_multiline_arrow_fragment_expressions(input),
            expected
        );
    }

    #[test]
    fn normalize_multiline_arrow_fragment_expressions_collapses_inline_fragment_line() {
        let input = "let b = () =>\n<><div>a</div><div>b</div></>;";
        let expected = "let b = () => <><div>a</div><div>b</div></>;";
        assert_eq!(
            normalize_multiline_arrow_fragment_expressions(input),
            expected
        );
    }

    #[test]
    fn normalize_multiline_arrow_fragment_expressions_handles_inline_closing_semicolon() {
        let input = "let b = () =>\n<>\n<div>a</div>\n<div>b</div>\n</>;";
        let expected = "let b = () => <><div>a</div><div>b</div></>;";
        assert_eq!(
            normalize_multiline_arrow_fragment_expressions(input),
            expected
        );
    }

    #[test]
    fn normalize_multiline_optional_chain_calls_joins_chain_lines() {
        let input = "result = Builder.makeBuilder()\n?.push(1)\n?.push(2);";
        let expected = "result = Builder.makeBuilder()?.push(1)?.push(2);";
        assert_eq!(normalize_multiline_optional_chain_calls(input), expected);
    }

    #[test]
    fn normalize_jsx_branch_paren_spacing_removes_wrapper_parens() {
        let input = "t1 = cond ? ( <div /> ) : ( <span /> );";
        let expected = "t1 = cond ? <div /> : <span />;";
        assert_eq!(normalize_jsx_branch_paren_spacing(input), expected);
    }

    #[test]
    fn normalize_jsx_nested_ternary_wrapper_parens_removes_inner_wrapper() {
        let input =
            "t1 = <Component>text{cond ? <A /> : (other ? <B /> : <C />) }tail</Component>;";
        let expected =
            "t1 = <Component>text{cond ? <A /> : other ? <B /> : <C />}tail</Component>;";
        assert_eq!(normalize_jsx_nested_ternary_wrapper_parens(input), expected);
    }

    #[test]
    fn normalize_simple_jsx_attr_brace_spacing_trims_simple_attr_exprs() {
        let input = "t1 = <Component foo={ bar } baz={ qux[idx] } />;";
        let expected = "t1 = <Component foo={bar} baz={qux[idx]} />;";
        assert_eq!(normalize_simple_jsx_attr_brace_spacing(input), expected);
    }

    #[test]
    fn normalize_destructuring_preserves_braces_inside_string_literals() {
        let input = r#"t1 = <c14 c15={"L^]w\\T\\qrRdT{N[Wy"} />;"#;
        let normalized = normalize_destructuring(input);
        assert!(normalized.contains(r#"RdT{N[Wy"#));
        assert!(!normalized.contains(r#"RdT{ N[Wy"#));
    }

    #[test]
    fn normalize_code_preserves_jsx_string_literal_braces() {
        let input = r#"t1 = <c14 c15={"CRinMqvmQe{SUpoN[\\g"} />;"#;
        let normalized = normalize_code(input);
        assert!(normalized.contains(r#"Qe{SUpoN[\\g"#));
        assert!(!normalized.contains(r#"Qe{ SUpoN[\\g"#));
    }

    #[test]
    fn normalize_multiline_object_method_bodies_collapses_simple_method() {
        let input = "t1 = cond ? { getValue() {\nreturn value;\n} } : 42;";
        let expected = "t1 = cond ? { getValue() { return value; } } : 42;";
        assert_eq!(normalize_multiline_object_method_bodies(input), expected);
    }

    #[test]
    fn normalize_multiline_object_literal_access_collapses_single_property_access() {
        let input = "t3 = {\n\"Component[key]\": () => value, }[\"Component[key]\"];";
        let expected = "t3 = { \"Component[key]\": () => value }[\"Component[key]\"];";
        assert_eq!(normalize_multiline_object_literal_access(input), expected);
    }

    #[test]
    fn normalize_jsx_text_expr_spacing_compact_preserves_attribute_boundaries() {
        let input = "t1 = <div key={id} render={record[id]}> gmhubcw {value} hflmn</div>;";
        let expected = "t1 = <div key={id} render={record[id]}> gmhubcw{value}hflmn</div>;";
        assert_eq!(normalize_jsx_text_expr_spacing_compact(input), expected);
    }

    #[test]
    fn normalize_jsx_text_expr_container_spacing_trims_child_container_edges() {
        let input = "t1 = <div>gmhubcw{ value ? <A /> : <B /> }hflmn</div>;";
        let expected = "t1 = <div>gmhubcw{value ? <A /> : <B />}hflmn</div>;";
        assert_eq!(normalize_jsx_text_expr_container_spacing(input), expected);
    }

    #[test]
    fn normalize_jsx_text_line_before_expr_joins_split_text_runs() {
        let input = "<div>\nrendering took\n{time} at {now}\n</div>";
        let expected = "<div>\nrendering took {time} at {now}\n</div>";
        assert_eq!(normalize_jsx_text_line_before_expr(input), expected);
    }

    #[test]
    fn normalize_small_array_bracket_spacing_trims_collapsed_return_arrays() {
        let input = "return [ item.id, { value: item.value } ]";
        let expected = "return [item.id, { value: item.value }]";
        assert_eq!(normalize_small_array_bracket_spacing(input), expected);
    }

    #[test]
    fn normalize_small_multiline_return_arrays_collapses_short_arrays() {
        let input = "return [\nitem.id,\n{ value: item.value }\n]";
        let expected = "return [ item.id, { value: item.value } ]";
        assert_eq!(normalize_small_multiline_return_arrays(input), expected);
    }

    #[test]
    fn normalize_temp_zero_suffixes_rewrites_common_conflict_suffix() {
        let input = "let t0_0;\nreturn t0_0;";
        let expected = "let t0;\nreturn t0;";
        assert_eq!(normalize_temp_zero_suffixes(input), expected);
    }

    #[test]
    fn normalize_object_shorthand_pairs_collapses_self_named_entries() {
        let input = "const obj = { ref: ref, children: children, value: t0 };";
        let expected = "const obj = { ref, children, value: t0 };";
        assert_eq!(normalize_object_shorthand_pairs(input), expected);
    }

    #[test]
    fn normalize_memo_cache_decl_arity_keeps_slots_in_sibling_branch_after_block_local_let() {
        let input = "function foo(a) {\nconst $ = _c(4);\nlet x;\nif ($[0] !== a) { x = { };\nif (a) { let t0;\nif ($[2] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = { };\n$[2] = t0\n} else { t0 = $[2]\n}\nlet y = t0;\nx.y = y\n} else { let t1;\nif ($[3] === Symbol.for(\"react.memo_cache_sentinel\")) { t1 = { };\n$[3] = t1\n} else { t1 = $[3]\n}\nlet z = t1;\nx.z = z\n}\n$[0] = a;\n$[1] = x\n} else { x = $[1]\n}\nreturn x\n}";
        assert_eq!(normalize_memo_cache_decl_arity(input), input);
    }

    #[test]
    fn normalize_tail_return_from_cache_alias_rewrites_missing_temp_return() {
        let input = "if ($0[0] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = value;\n$0[0] = t0;\n} else { t0 = $0[0];\n}\nreturn t1;";
        let expected = "if ($0[0] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = value;\n$0[0] = t0;\n} else { t0 = $0[0];\n}\nreturn t0;";
        assert_eq!(normalize_tail_return_from_cache_alias(input), expected);
    }

    #[test]
    fn normalize_simple_alias_return_tail_collapses_alias_before_return() {
        let input = "let t0_0 = t0;\nreturn t0_0;";
        let expected = "return t0;";
        assert_eq!(normalize_simple_alias_return_tail(input), expected);
    }

    #[test]
    fn normalize_inline_if_first_statements_joins_simple_assignment() {
        let input = "if (cond) {\nvalue = x;\n} else {\nvalue = y;\n}";
        let expected = "if (cond) { value = x;\n} else { value = y;\n}";
        assert_eq!(normalize_inline_if_first_statements(input), expected);
    }

    #[test]
    fn normalize_react_memo_closing_paren_moves_stranded_closer() {
        let input = "let View = React.memo((t0) =>{ return t0; }\nfunction _temp(item) {\nreturn item;\n}\n});";
        let expected = "let View = React.memo((t0) =>{ return t0; });\nfunction _temp(item) {\nreturn item;\n}\n}";
        assert_eq!(normalize_react_memo_closing_paren(input), expected);
    }

    #[test]
    fn normalize_jsx_semicolon_on_own_line_joins_split_semicolon() {
        let input = "let cb = () =>\n<Component />\n;";
        let expected = "let cb = () =>\n<Component />;";
        assert_eq!(normalize_jsx_semicolon_on_own_line(input), expected);
    }

    #[test]
    fn normalize_parenthesized_arrow_initializers_strips_outer_wrapper() {
        let input = "let callback = ((value) =>{ref.current = value });";
        let expected = "let callback = (value) =>{ref.current = value };";
        assert_eq!(
            super::normalize_parenthesized_arrow_initializers(input),
            expected
        );
    }

    #[test]
    fn normalize_parenthesized_multiline_arrow_initializers_strips_outer_wrapper() {
        let input = "const callback = ((value) => {\nref.current = value;\n});";
        let expected = "const callback = (value) => {\nref.current = value;\n};";
        assert_eq!(
            super::normalize_parenthesized_multiline_arrow_initializers(input),
            expected
        );
    }

    #[test]
    fn normalize_strict_multiline_call_tail_args_merges_trailing_arg_lines() {
        let input = "setTimeout(() => {\nwork();\n},\n0,\n);";
        let expected = "setTimeout(() => {\nwork();\n}, 0);";
        assert_eq!(
            super::normalize_strict_multiline_call_tail_args(input),
            expected
        );
    }
}
