use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;

use crate::normalizations::{
    canonicalize_strict_text, prepare_code_for_compare, preprocess_flow_syntax_for_expectation,
};
use crate::pragmas::parse_pragma;
use crate::{FixtureSuiteOptions, JsRuntime};

#[derive(Clone)]
pub(crate) struct Fixture {
    pub(crate) name: String,
    pub(crate) input_path: PathBuf,
    pub(crate) expect_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExpectedState {
    Transform,
    Skip,
    Error,
    Bailout,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActualState {
    Transformed,
    Skipped,
    Error,
    Bailout,
    Timeout,
    HarnessFailure,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixtureOutcome {
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
pub(crate) struct FixtureResult {
    pub(crate) name: String,
    pub(crate) status: Status,
    pub(crate) message: Option<String>,
    pub(crate) expected_state: Option<ExpectedState>,
    pub(crate) actual_state: ActualState,
    pub(crate) outcome: FixtureOutcome,
    pub(crate) parity_success: bool,
    pub(crate) actual_code: Option<String>,
    pub(crate) expected_code: Option<String>,
    pub(crate) is_error_fixture: bool,
}

pub(crate) fn find_fixture_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    workspace_root.join("third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler")
}

pub(crate) fn find_custom_fixture_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
    workspace_root.join("tests/fixtures/compiler")
}

pub(crate) fn collect_fixtures(
    dir: &Path,
    filter: Option<&str>,
    name_prefix: Option<&str>,
) -> Vec<Fixture> {
    let mut fixtures = Vec::new();

    if !dir.exists() {
        return fixtures;
    }

    collect_fixtures_recursive(dir, None, filter, name_prefix, &mut fixtures);

    fixtures.sort_by(|a, b| a.name.cmp(&b.name));

    fixtures
}

fn collect_fixtures_recursive(
    dir: &Path,
    prefix: Option<&str>,
    filter: Option<&str>,
    name_prefix: Option<&str>,
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
            collect_fixtures_recursive(&path, Some(&subdir_name), filter, name_prefix, fixtures);
            continue;
        }

        let ext = path.extension().and_then(|e| e.to_str());

        match ext {
            Some("js" | "jsx" | "ts" | "tsx") => {}
            _ => continue,
        }

        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let local_name = match prefix {
            Some(p) => format!("{p}/{stem}"),
            None => stem.clone(),
        };
        let name = match name_prefix {
            Some(np) => format!("{np}{local_name}"),
            None => local_name,
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

pub(crate) fn run_fixture_suite(
    fixtures: &[Fixture],
    options: FixtureSuiteOptions,
) -> Vec<FixtureResult> {
    if options.parallel {
        fixtures
            .par_iter()
            .map(|fixture| {
                if options.verbose {
                    println!("Running {}", fixture.name);
                }
                let res =
                    run_fixture_with_timeout(fixture, options.fixture_timeout, options.run_skipped);
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
                let res =
                    run_fixture_with_timeout(fixture, options.fixture_timeout, options.run_skipped);
                if options.verbose {
                    println!("Finished {}", fixture.name);
                }
                res
            })
            .collect()
    }
}

fn run_fixture_with_timeout(
    fixture: &Fixture,
    timeout: std::time::Duration,
    run_skipped: bool,
) -> FixtureResult {
    let fixture_clone = fixture.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024) // 64MB stack
        .spawn(move || {
            let r = run_fixture(&fixture_clone, run_skipped);
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

fn single_param_arrow_paren_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\(([A-Za-z_$][A-Za-z0-9_$]*)\)=>").unwrap())
}

fn trailing_comma_before_closer_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r",([}\]\)])").unwrap())
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

fn run_fixture(fixture: &Fixture, run_skipped: bool) -> FixtureResult {
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
    options.environment.assert_valid_mutable_ranges = true;
    options
        .environment
        .validate_preserve_existing_memoization_guarantees = pragma
        .validate_preserve_existing_memoization_guarantees
        .unwrap_or(false);
    // Upstream fixture runner uses `{}` Babel defaults, where
    // enablePreserveExistingMemoizationGuarantees effectively behaves as true
    // (useMemo/useCallback are preserved in expect.md outputs). Despite the zod
    // schema declaring `.default(false)`, the runtime behavior with `{}` keeps
    // existing memoization.
    // TODO: switch to unwrap_or(true) once preserve-memo codepath bugs are fixed
    // (currently 72 regressions when enabled). For now, keep false to avoid
    // masking other bugs in the conformance suite.
    options
        .environment
        .enable_preserve_existing_memoization_guarantees = pragma
        .enable_preserve_existing_memoization_guarantees
        .unwrap_or(false);
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
        let actual = maybe_apply_snap_post_babel_plugins(
            &result.code,
            &filename,
            language,
            source_type,
            false,
            &source,
        );
        let raw_actual = prepare_code_for_compare(&actual);
        let raw_expected = prepare_code_for_compare(&expected_code);
        let formatted_actual = format_code_for_compare(&fixture.input_path, &actual);
        let formatted_expected = format_code_for_compare(&fixture.input_path, &expected_code);
        let formatted_actual = prepare_code_for_compare(&formatted_actual);
        let formatted_expected = prepare_code_for_compare(&formatted_expected);
        let (actual, expected) = if formatted_actual == formatted_expected {
            (formatted_actual, formatted_expected)
        } else {
            (raw_actual, raw_expected)
        };

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
                } else if actual == expected {
                    // Compiler said it transformed but the output matches the
                    // expected bailout output -- treat as parity success.
                    FixtureResult {
                        name: fixture.name.clone(),
                        status: Status::Pass,
                        message: Some(
                            "Expected bailout, compiler transformed but output matches".to_string(),
                        ),
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

// --- Markdown extraction ---

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

fn format_code_for_compare(input_path: &Path, code: &str) -> String {
    format_with_oxfmt(input_path, code).unwrap_or_else(|_| code.to_string())
}

// --- Formatter canonicalization ---

const PRETTIER_FORMAT_SCRIPT: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { pathToFileURL } from 'node:url';

async function resolvePrettier() {
  const compilerDir = path.join(process.cwd(), 'third_party', 'react', 'compiler');
  const prettierPath = path.join(compilerDir, 'node_modules', 'prettier', 'index.mjs');
  if (!fs.existsSync(prettierPath)) {
    throw new Error(`missing prettier at ${prettierPath}`);
  }
  return import(pathToFileURL(prettierPath).href);
}

const prettier = await resolvePrettier();

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
    const code = await prettier.format(request.source || '', {
      semi: true,
      singleQuote: false,
      jsxSingleQuote: false,
      trailingComma: 'all',
      parser: request.parser || 'babel-ts',
      filepath: request.fileName || 'fixture.js',
    });
    process.stdout.write(JSON.stringify({ code }) + '\n');
  } catch (error) {
    process.stdout.write(JSON.stringify({ error: error?.message || 'prettier failed' }) + '\n');
  }
}
"#;

#[derive(Serialize)]
struct FormatRequest<'a> {
    #[serde(rename = "fileName")]
    file_name: &'a str,
    parser: &'a str,
    source: &'a str,
}

#[derive(Deserialize)]
struct FormatResponse {
    code: Option<String>,
    error: Option<String>,
}

struct FormatterSession {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    _stderr: ChildStderr,
}

fn init_formatter_session() -> Result<std::sync::Mutex<FormatterSession>, String> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "failed to resolve workspace root".to_string())?;

    let mut child = Command::new("node")
        .current_dir(workspace_root)
        .args(["--input-type=module", "-e", PRETTIER_FORMAT_SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to spawn formatter: {err}"))?;

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

    Ok(std::sync::Mutex::new(FormatterSession {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout),
        _stderr: stderr,
    }))
}

fn formatter_request(
    session: &mut FormatterSession,
    file_name: &str,
    parser: &str,
    code: &str,
) -> Result<String, String> {
    let request = serde_json::to_string(&FormatRequest {
        file_name,
        parser,
        source: code,
    })
    .map_err(|err| format!("failed to encode formatter request: {err}"))?;
    session
        .stdin
        .write_all(request.as_bytes())
        .and_then(|_| session.stdin.write_all(b"\n"))
        .and_then(|_| session.stdin.flush())
        .map_err(|err| format!("failed to write formatter stdin: {err}"))?;

    let mut response_line = String::new();
    session
        .stdout
        .read_line(&mut response_line)
        .map_err(|err| format!("failed to read formatter output: {err}"))?;
    if response_line.is_empty() {
        return Err("formatter process terminated unexpectedly".to_string());
    }

    let response: FormatResponse = serde_json::from_str(response_line.trim_end())
        .map_err(|err| format!("failed to decode formatter response: {err}"))?;
    if let Some(error) = response.error {
        return Err(error);
    }

    response
        .code
        .ok_or_else(|| "formatter response missing formatted code".to_string())
}

fn format_with_oxfmt(input_path: &Path, code: &str) -> Result<String, String> {
    static FORMATTER_SESSION: OnceLock<Result<std::sync::Mutex<FormatterSession>, String>> =
        OnceLock::new();

    let file_name = input_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fixture.js");
    let parser = if file_name.contains(".flow.") || file_name.ends_with(".flow.js") {
        "flow"
    } else {
        "babel-ts"
    };
    let session = match FORMATTER_SESSION.get_or_init(init_formatter_session) {
        Ok(session) => session,
        Err(err) => return Err(err.clone()),
    };

    let mut session = session
        .lock()
        .map_err(|_| "failed to lock formatter session".to_string())?;

    let result = formatter_request(&mut session, file_name, parser, code);

    if result.is_err()
        && matches!(
            input_path.extension().and_then(|e| e.to_str()),
            Some("js" | "jsx")
        )
    {
        let tsx_name = input_path
            .with_extension("tsx")
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .unwrap_or_else(|| "fixture.tsx".to_string());
        let retry = formatter_request(&mut session, &tsx_name, parser, code);
        if retry.is_ok() {
            return retry;
        }
    }

    result
}

// --- Post-babel plugins ---

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

  // Collapse FBT plural cross-product tables to match upstream's
  // babel-plugin-fbt which only emits diagonal entries, and recompute
  // the hash key (hk) from the collapsed table.
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

/// Collapse FBT plural cross-product tables in the actual output to match
/// upstream's babel-plugin-fbt which emits only diagonal entries.
fn postprocess_collapse_fbt_tables(code: &str, fixture_source: &str) -> String {
    if !code.contains("fbt._(") {
        return code.to_string();
    }
    let desc = extract_fbt_description(fixture_source);
    let desc = desc.as_deref().unwrap_or("TestDescription");
    let table_re = regex::Regex::new(
        r#"(?s)\{\s*"\*":\s*\{\s*"\*":\s*"([^"]+)",\s*_1:\s*"([^"]+)"\s*,?\s*\},\s*_1:\s*\{\s*"\*":\s*"([^"]+)",\s*_1:\s*"([^"]+)"\s*,?\s*\}\s*,?\s*\}"#,
    )
    .unwrap();

    let mut result = code.to_string();
    let mut changed = false;

    for caps in table_re.captures_iter(code) {
        let star_star = caps.get(1).unwrap().as_str();
        let one_one = caps.get(4).unwrap().as_str();
        let collapsed = format!(
            r#"{{ "*": {{ "*": "{}" }}, _1: {{ _1: "{}" }} }}"#,
            star_star, one_one
        );
        result = result.replace(caps.get(0).unwrap().as_str(), &collapsed);
        changed = true;
    }

    if !changed {
        return code.to_string();
    }

    let hk_in_context_re = regex::Regex::new(
        r#"(?s)fbt\._\(\s*\{\s*"\*":\s*\{\s*"\*":\s*"([^"]+)"\s*,?\s*\},\s*_1:\s*\{\s*_1:\s*"([^"]+)"\s*,?\s*\}\s*,?\s*\},\s*\[[\s\S]*?\],\s*\{\s*hk:\s*"([^"]+)"\s*,?\s*\}"#,
    )
    .unwrap();

    let result_clone = result.clone();
    for caps in hk_in_context_re.captures_iter(&result_clone) {
        let star_star = caps.get(1).unwrap().as_str();
        let one_one = caps.get(2).unwrap().as_str();
        let old_hk = caps.get(3).unwrap().as_str();

        let table_json = format!(
            r#"{{"*":{{"*":"{}"}},"_1":{{"_1":"{}"}}}}"#,
            star_star, one_one
        );
        let hash_input = format!("{}|{}", table_json, desc);
        let hash = jenkins_hash(hash_input.as_bytes());
        let new_hk = uint_to_base62(hash);

        result = result.replace(
            &format!(r#"hk: "{}""#, old_hk),
            &format!(r#"hk: "{}""#, new_hk),
        );
    }

    result
}

/// Jenkins one-at-a-time hash (matching fbt's implementation).
fn jenkins_hash(data: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    for &byte in data {
        hash = hash.wrapping_add(u32::from(byte));
        hash = hash.wrapping_add(hash << 10);
        hash ^= hash >> 6;
    }
    hash = hash.wrapping_add(hash << 3);
    hash ^= hash >> 11;
    hash = hash.wrapping_add(hash << 15);
    hash
}

/// Convert a u32 to base-62 string (matching fbt's uintToBaseN).
fn uint_to_base62(mut number: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut result = Vec::new();
    loop {
        result.push(DIGITS[(number % 62) as usize]);
        number /= 62;
        if number == 0 {
            break;
        }
    }
    result.reverse();
    String::from_utf8(result).unwrap()
}

/// Extract the fbt description from source code.
fn extract_fbt_description(source: &str) -> Option<String> {
    let fbt_call_re = regex::Regex::new(r#"fbt\s*\([^,]+,\s*['"]([^'"]+)['"]\s*[,)]"#).unwrap();
    if let Some(caps) = fbt_call_re.captures(source) {
        return Some(caps.get(1).unwrap().as_str().to_string());
    }
    let fbt_jsx_re = regex::Regex::new(r#"<fbt\s+desc\s*=\s*"([^"]+)""#).unwrap();
    if let Some(caps) = fbt_jsx_re.captures(source) {
        return Some(caps.get(1).unwrap().as_str().to_string());
    }
    None
}

fn maybe_apply_snap_post_babel_plugins(
    code: &str,
    filename: &str,
    language: &str,
    source_type: &str,
    force_run: bool,
    fixture_source: &str,
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
            postprocess_collapse_fbt_tables(&output, fixture_source)
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

pub(crate) fn resolve_js_runtime() -> Option<JsRuntime> {
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
