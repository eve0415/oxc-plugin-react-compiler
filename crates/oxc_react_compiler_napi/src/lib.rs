//! N-API bindings for oxc_react_compiler.

use napi_derive::napi;

// ── Shared Environment Config ────────────────────────────────────

#[napi(object)]
pub struct NapiEnvironmentConfig {
    // Validation flags
    pub validate_hooks_usage: Option<bool>,
    pub validate_ref_access_during_render: Option<bool>,
    pub validate_no_set_state_in_render: Option<bool>,
    pub validate_no_set_state_in_effects: Option<bool>,
    pub validate_no_derived_computations_in_effects: Option<bool>,
    pub validate_no_jsx_in_try_statements: Option<bool>,
    pub validate_static_components: Option<bool>,
    pub validate_memoized_effect_dependencies: Option<bool>,
    pub validate_no_capitalized_calls: Option<Vec<String>>,
    pub validate_no_impure_functions_in_render: Option<bool>,
    pub validate_no_freezing_known_mutable_functions: Option<bool>,
    pub validate_no_void_use_memo: Option<bool>,
    pub validate_blocklisted_imports: Option<Vec<String>>,
    pub validate_preserve_existing_memoization_guarantees: Option<bool>,
    pub assert_valid_mutable_ranges: Option<bool>,
    pub validate_no_dynamically_created_components_or_hooks: Option<bool>,

    // Feature flags
    pub enable_emit_freeze: Option<bool>,
    pub enable_emit_hook_guards: Option<bool>,
    pub enable_instruction_reordering: Option<bool>,
    pub enable_function_outlining: Option<bool>,
    pub enable_jsx_outlining: Option<bool>,
    pub enable_emit_instrument_forget: Option<bool>,
    pub enable_change_variable_codegen: Option<bool>,
    pub enable_memoization_comments: Option<bool>,
    pub enable_fire: Option<bool>,
    pub enable_name_anonymous_functions: Option<bool>,
    pub enable_preserve_existing_memoization_guarantees: Option<bool>,
    pub enable_preserve_existing_manual_use_memo: Option<bool>,
    pub enable_use_type_annotations: Option<bool>,
    pub enable_optional_dependencies: Option<bool>,
    pub enable_assume_hooks_follow_rules_of_react: Option<bool>,
    pub enable_transitively_freeze_function_expressions: Option<bool>,
    pub enable_treat_function_deps_as_conditional: Option<bool>,
    pub enable_treat_ref_like_identifiers_as_refs: Option<bool>,
    pub enable_treat_set_identifiers_as_state_setters: Option<bool>,
    pub enable_custom_type_definition_for_reanimated: Option<bool>,
    pub enable_allow_set_state_from_refs_in_effects: Option<bool>,
    pub disable_memoization_for_debugging: Option<bool>,
    pub enable_new_mutation_aliasing_model: Option<bool>,
    pub enable_propagate_deps_in_hir: Option<bool>,
    pub enable_reactive_scopes_in_hir: Option<bool>,
    pub enable_change_detection_for_debugging: Option<bool>,
    pub enable_reset_cache_on_source_file_changes: Option<bool>,
    pub throw_unknown_exception_testonly: Option<bool>,

    // Complex configs
    pub hook_pattern: Option<String>,
    pub infer_effect_dependencies: Option<Vec<NapiInferEffectDepsConfig>>,
    pub inline_jsx_transform: Option<NapiInlineJsxTransformConfig>,
    pub lower_context_access: Option<NapiLowerContextAccessConfig>,
    pub custom_macros: Option<Vec<NapiCustomMacroConfig>>,
}

/// Upstream-compatible shape: `{ function: { source, importSpecifierName }, autodepsIndex }`
#[napi(object)]
pub struct NapiInferEffectDepsConfig {
    pub function: NapiExternalFunction,
    pub autodeps_index: u32,
}

#[napi(object)]
pub struct NapiExternalFunction {
    pub source: String,
    pub import_specifier_name: String,
}

#[napi(object)]
pub struct NapiInlineJsxTransformConfig {
    pub element_symbol: String,
    pub global_dev_var: String,
}

#[napi(object)]
pub struct NapiLowerContextAccessConfig {
    pub module: String,
    pub imported_name: String,
}

#[napi(object)]
pub struct NapiGatingConfig {
    pub source: String,
    pub import_specifier_name: String,
}

#[napi(object)]
pub struct NapiDynamicGatingConfig {
    pub source: String,
}

/// Custom macro config. `props` is `['*']` for wildcard, `['name']` for named property.
#[napi(object)]
pub struct NapiCustomMacroConfig {
    pub name: String,
    pub props: Vec<String>,
}

fn merge_env_config(
    napi: Option<NapiEnvironmentConfig>,
) -> oxc_react_compiler::options::EnvironmentConfig {
    use oxc_react_compiler::options::EnvironmentConfig;
    let mut env = EnvironmentConfig::default();
    let Some(n) = napi else { return env };

    macro_rules! merge {
        ($field:ident) => {
            if let Some(v) = n.$field {
                env.$field = v;
            }
        };
    }

    // Validation flags
    merge!(validate_hooks_usage);
    merge!(validate_ref_access_during_render);
    merge!(validate_no_set_state_in_render);
    merge!(validate_no_set_state_in_effects);
    merge!(validate_no_derived_computations_in_effects);
    merge!(validate_no_jsx_in_try_statements);
    merge!(validate_static_components);
    merge!(validate_memoized_effect_dependencies);
    merge!(validate_no_impure_functions_in_render);
    merge!(validate_no_freezing_known_mutable_functions);
    merge!(validate_no_void_use_memo);
    merge!(validate_preserve_existing_memoization_guarantees);
    merge!(assert_valid_mutable_ranges);
    merge!(validate_no_dynamically_created_components_or_hooks);

    // Option<Vec<String>> fields
    if let Some(v) = n.validate_no_capitalized_calls {
        env.validate_no_capitalized_calls = Some(v);
    }
    if let Some(v) = n.validate_blocklisted_imports {
        env.validate_blocklisted_imports = Some(v);
    }

    // Feature flags
    merge!(enable_emit_freeze);
    merge!(enable_emit_hook_guards);
    merge!(enable_instruction_reordering);
    merge!(enable_function_outlining);
    merge!(enable_jsx_outlining);
    merge!(enable_emit_instrument_forget);
    merge!(enable_change_variable_codegen);
    merge!(enable_memoization_comments);
    merge!(enable_fire);
    merge!(enable_name_anonymous_functions);
    merge!(enable_preserve_existing_memoization_guarantees);
    merge!(enable_preserve_existing_manual_use_memo);
    merge!(enable_use_type_annotations);
    merge!(enable_optional_dependencies);
    merge!(enable_assume_hooks_follow_rules_of_react);
    merge!(enable_transitively_freeze_function_expressions);
    merge!(enable_treat_function_deps_as_conditional);
    merge!(enable_treat_ref_like_identifiers_as_refs);
    merge!(enable_treat_set_identifiers_as_state_setters);
    merge!(enable_custom_type_definition_for_reanimated);
    merge!(enable_allow_set_state_from_refs_in_effects);
    merge!(disable_memoization_for_debugging);
    merge!(enable_new_mutation_aliasing_model);
    merge!(enable_propagate_deps_in_hir);
    merge!(enable_reactive_scopes_in_hir);
    merge!(enable_change_detection_for_debugging);
    merge!(throw_unknown_exception_testonly);

    // Option<bool> field
    if let Some(v) = n.enable_reset_cache_on_source_file_changes {
        env.enable_reset_cache_on_source_file_changes = Some(v);
    }

    // Hook pattern
    if let Some(v) = n.hook_pattern {
        env.hook_pattern = Some(v);
    }

    // Complex configs
    if let Some(v) = n.infer_effect_dependencies {
        use oxc_react_compiler::options::InferEffectDepsConfig;
        env.infer_effect_dependencies = Some(
            v.into_iter()
                .map(|c| InferEffectDepsConfig {
                    function_module: c.function.source,
                    function_name: c.function.import_specifier_name,
                    autodeps_index: c.autodeps_index as usize,
                })
                .collect(),
        );
    }
    if let Some(v) = n.inline_jsx_transform {
        use oxc_react_compiler::options::InlineJsxTransformConfig;
        env.inline_jsx_transform = Some(InlineJsxTransformConfig {
            element_symbol: v.element_symbol,
            global_dev_var: v.global_dev_var,
        });
    }
    if let Some(v) = n.lower_context_access {
        use oxc_react_compiler::options::LowerContextAccessConfig;
        env.lower_context_access = Some(LowerContextAccessConfig {
            module: v.module,
            imported_name: v.imported_name,
        });
    }
    if let Some(v) = n.custom_macros {
        use oxc_react_compiler::options::{CustomMacroConfig, MacroProp};
        env.custom_macros = Some(
            v.into_iter()
                .map(|c| CustomMacroConfig {
                    name: c.name,
                    props: c
                        .props
                        .into_iter()
                        .map(|p| {
                            if p == "*" {
                                MacroProp::Wildcard
                            } else {
                                MacroProp::Name(p)
                            }
                        })
                        .collect(),
                })
                .collect(),
        );
    }

    env
}

// ── Transform API ────────────────────────────────────────────────

#[napi(object)]
pub struct TransformOptions {
    #[napi(ts_type = "'infer' | 'syntax' | 'annotation' | 'all'")]
    pub compilation_mode: Option<String>,
    #[napi(ts_type = "'none' | 'all'")]
    pub panic_threshold: Option<String>,
    pub target: Option<String>,
    /// Whether to generate source maps. Defaults to `true`.
    pub source_map: Option<bool>,
    /// Environment configuration for validation and feature flags.
    pub environment: Option<NapiEnvironmentConfig>,
    pub gating: Option<NapiGatingConfig>,
    pub dynamic_gating: Option<NapiDynamicGatingConfig>,
}

#[napi(object)]
pub struct TransformResult {
    pub transformed: bool,
    pub code: String,
    pub map: Option<String>,
}

#[napi]
pub fn transform(
    filename: String,
    source: String,
    options: Option<TransformOptions>,
) -> TransformResult {
    let opts = parse_transform_options(options);
    let result = oxc_react_compiler::compile(&filename, &source, &opts);
    TransformResult {
        transformed: result.transformed,
        code: result.code,
        map: result.map,
    }
}

fn parse_transform_options(
    options: Option<TransformOptions>,
) -> oxc_react_compiler::options::PluginOptions {
    let Some(opts) = options else {
        return oxc_react_compiler::options::PluginOptions::default();
    };

    use oxc_react_compiler::options::*;

    let compilation_mode = match opts.compilation_mode.as_deref() {
        Some("syntax") => CompilationMode::Syntax,
        Some("annotation") => CompilationMode::Annotation,
        Some("all") => CompilationMode::All,
        _ => CompilationMode::Infer,
    };

    let panic_threshold = match opts.panic_threshold.as_deref() {
        Some("all") => PanicThreshold::All,
        _ => PanicThreshold::None,
    };

    PluginOptions {
        compilation_mode,
        panic_threshold,
        target: opts.target.unwrap_or_else(|| "19".to_string()),
        environment: merge_env_config(opts.environment),
        gating: opts.gating.map(|g| GatingConfig {
            source: g.source,
            import_specifier_name: g.import_specifier_name,
        }),
        dynamic_gating: opts
            .dynamic_gating
            .map(|g| DynamicGatingConfig { source: g.source }),
        source_map: opts.source_map.unwrap_or(true),
        ..PluginOptions::default()
    }
}

// ── Lint API ─────────────────────────────────────────────────────

#[napi(object)]
pub struct NapiLintOptions {
    #[napi(ts_type = "'infer' | 'syntax' | 'annotation' | 'all'")]
    pub compilation_mode: Option<String>,
    #[napi(ts_type = "'none' | 'all'")]
    pub panic_threshold: Option<String>,
    pub target: Option<String>,
    pub environment: Option<NapiEnvironmentConfig>,
    pub custom_opt_out_directives: Option<Vec<String>>,
    pub ignore_use_no_forget: Option<bool>,
    pub eslint_suppression_rules: Option<Vec<String>>,
    pub flow_suppressions: Option<bool>,
    pub gating: Option<NapiGatingConfig>,
    pub dynamic_gating: Option<NapiDynamicGatingConfig>,
}

fn parse_lint_options(
    options: Option<NapiLintOptions>,
) -> oxc_react_compiler::options::PluginOptions {
    use oxc_react_compiler::options::*;

    let Some(opts) = options else {
        return lint_defaults();
    };

    let compilation_mode = match opts.compilation_mode.as_deref() {
        Some("syntax") => CompilationMode::Syntax,
        Some("annotation") => CompilationMode::Annotation,
        Some("all") => CompilationMode::All,
        Some("infer") => CompilationMode::Infer,
        _ => CompilationMode::Infer,
    };
    let panic_threshold = match opts.panic_threshold.as_deref() {
        Some("all") => PanicThreshold::All,
        _ => PanicThreshold::None,
    };

    PluginOptions {
        compilation_mode,
        panic_threshold,
        target: opts.target.unwrap_or_else(|| "19".to_string()),
        environment: merge_env_config(opts.environment),
        custom_opt_out_directives: opts.custom_opt_out_directives.unwrap_or_default(),
        ignore_use_no_forget: opts.ignore_use_no_forget.unwrap_or(false),
        eslint_suppression_rules: opts.eslint_suppression_rules,
        flow_suppressions: opts.flow_suppressions.unwrap_or(false),
        gating: opts.gating.map(|g| GatingConfig {
            source: g.source,
            import_specifier_name: g.import_specifier_name,
        }),
        dynamic_gating: opts
            .dynamic_gating
            .map(|g| DynamicGatingConfig { source: g.source }),
        no_emit: true,
        source_map: false,
    }
}

/// Default lint options matching upstream eslint-plugin-react-compiler COMPILER_OPTIONS.
fn lint_defaults() -> oxc_react_compiler::options::PluginOptions {
    use oxc_react_compiler::options::*;
    PluginOptions {
        compilation_mode: CompilationMode::Infer,
        panic_threshold: PanicThreshold::None,
        no_emit: true,
        flow_suppressions: false,
        source_map: false,
        environment: EnvironmentConfig {
            validate_hooks_usage: true,
            validate_ref_access_during_render: true,
            validate_no_set_state_in_render: true,
            validate_no_set_state_in_effects: true,
            validate_no_derived_computations_in_effects: true,
            validate_no_jsx_in_try_statements: true,
            validate_no_impure_functions_in_render: true,
            validate_static_components: true,
            validate_no_freezing_known_mutable_functions: true,
            validate_no_void_use_memo: true,
            validate_no_capitalized_calls: Some(Vec::new()),
            ..EnvironmentConfig::default()
        },
        ..PluginOptions::default()
    }
}

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintRelated {
    pub message: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintSuggestion {
    pub description: String,
    /// "insert-before" | "insert-after" | "remove" | "replace"
    pub op: String,
    pub range_start: u32,
    pub range_end: u32,
    pub text: Option<String>,
}

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintDiagnostic {
    /// ErrorCategory string value (e.g., "Hooks", "Purity")
    pub category: String,
    pub message: String,
    /// "error" | "warning" | "hint"
    pub severity: String,
    pub start_line: Option<u32>,
    pub start_column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
    pub related: Vec<NapiLintRelated>,
    pub suggestions: Vec<NapiLintSuggestion>,
}

// ── Conversion helpers ───────────────────────────────────────────

fn convert_diagnostic(diag: oxc_react_compiler::error::LintDiagnostic) -> NapiLintDiagnostic {
    // Enrich message with related diagnostic locations (always-on)
    let message = if diag.related.is_empty() {
        diag.message
    } else {
        let mut msg = diag.message;
        for r in &diag.related {
            use std::fmt::Write;
            write!(
                msg,
                " (see also: line {}, col {} -- {})",
                r.start_line, r.start_column, r.message
            )
            .unwrap();
        }
        msg
    };

    NapiLintDiagnostic {
        category: format!("{:?}", diag.category),
        message,
        severity: diag.severity.to_string(),
        start_line: if diag.has_location {
            Some(diag.start_line)
        } else {
            None
        },
        start_column: if diag.has_location {
            Some(diag.start_column)
        } else {
            None
        },
        end_line: if diag.has_location {
            Some(diag.end_line)
        } else {
            None
        },
        end_column: if diag.has_location {
            Some(diag.end_column)
        } else {
            None
        },
        related: diag
            .related
            .into_iter()
            .map(|r| NapiLintRelated {
                message: r.message,
                start_line: r.start_line,
                start_column: r.start_column,
                end_line: r.end_line,
                end_column: r.end_column,
            })
            .collect(),
        suggestions: diag
            .suggestions
            .into_iter()
            .map(|s| NapiLintSuggestion {
                description: s.description,
                op: s.op.to_string(),
                range_start: s.range.0,
                range_end: s.range.1,
                text: s.text,
            })
            .collect(),
    }
}

#[napi]
pub fn lint(
    filename: String,
    source: String,
    options: Option<NapiLintOptions>,
) -> Vec<NapiLintDiagnostic> {
    let opts = parse_lint_options(options);
    let diagnostics = oxc_react_compiler::lint(&filename, &source, &opts);
    diagnostics.into_iter().map(convert_diagnostic).collect()
}
