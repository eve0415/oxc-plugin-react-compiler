//! Plugin options.
//!
//! Port of `PluginOptions.ts` and related config from upstream.

/// How the compiler decides which functions to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompilationMode {
    /// Compile functions that React can use (components and hooks), inferred by usage.
    #[default]
    Infer,
    /// Compile functions annotated with `"use memo"` directive.
    Annotation,
    /// Compile all top-level functions.
    All,
}

/// When to panic (throw) vs silently bail out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanicThreshold {
    /// Never panic; always bail out silently.
    #[default]
    None,
    /// Panic on all errors (useful for testing).
    All,
}

/// Configuration for gating compiled output behind a feature flag import.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatingConfig {
    pub source: String,
    pub import_specifier_name: String,
}

/// Configuration for dynamic gating via `use memo if(...)`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DynamicGatingConfig {
    pub source: String,
}

/// Top-level plugin options.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PluginOptions {
    pub compilation_mode: CompilationMode,
    pub panic_threshold: PanicThreshold,
    /// React version target (e.g., "19").
    pub target: String,
    /// Environment configuration for hook/function typing.
    pub environment: EnvironmentConfig,
    /// Additional opt-out directives beyond the default "use no forget" / "use no memo".
    /// Set via `@customOptOutDirectives:["directive1", "directive2"]` pragma.
    pub custom_opt_out_directives: Vec<String>,
    /// When true, ignore "use no forget" / "use no memo" directives.
    /// Set via `@ignoreUseNoForget` pragma.
    pub ignore_use_no_forget: bool,
    /// When set, compile and emit a gated version of the function.
    /// Set via `@gating` pragma.
    pub gating: Option<GatingConfig>,
    /// When set, enables dynamic gating via `use memo if(...)`.
    /// Set via `@dynamicGating:{"source":"..."}` pragma.
    pub dynamic_gating: Option<DynamicGatingConfig>,
    /// When true, skip codegen but still analyze/lint.
    /// Set via `@noEmit` pragma.
    pub no_emit: bool,
    /// Custom ESLint suppression rule names. When set, code suppressing these rules
    /// will skip compilation. Empty vec means never bail out.
    /// Set via `@eslintSuppressionRules:["rule1", "rule2"]` pragma.
    pub eslint_suppression_rules: Option<Vec<String>>,
    /// Whether to report suppression errors for Flow suppressions.
    /// Set via `@flowSuppressions` / `@enableFlowSuppressions` pragma.
    pub flow_suppressions: bool,
    /// Whether to generate source maps for transformed output.
    pub source_map: bool,
}

impl Default for PluginOptions {
    fn default() -> Self {
        Self {
            compilation_mode: CompilationMode::default(),
            panic_threshold: PanicThreshold::default(),
            target: "19".to_string(),
            environment: EnvironmentConfig::default(),
            custom_opt_out_directives: Vec::new(),
            ignore_use_no_forget: false,
            gating: None,
            dynamic_gating: None,
            no_emit: false,
            eslint_suppression_rules: None,
            flow_suppressions: true, // upstream default is true
            source_map: true,
        }
    }
}

/// Environment configuration — controls how the compiler types known APIs.
///
/// Port of `EnvironmentConfigSchema` from upstream `Environment.ts`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EnvironmentConfig {
    // --- Validation flags ---
    /// Enable validation of hooks usage (rules of hooks).
    pub validate_hooks_usage: bool,
    /// Validate that ref values (`ref.current`) are not accessed during render.
    pub validate_ref_access_during_render: bool,
    /// Validate that setState is not unconditionally called during render.
    pub validate_no_set_state_in_render: bool,
    /// Validate that setState is not called synchronously within an effect.
    pub validate_no_set_state_in_effects: bool,
    /// Validate that effects are not used to calculate derived data.
    pub validate_no_derived_computations_in_effects: bool,
    /// Validate against creating JSX within a try block.
    pub validate_no_jsx_in_try_statements: bool,
    /// Validate against dynamically creating components during render.
    pub validate_static_components: bool,
    /// Validate that dependencies of effect hooks are memoized.
    pub validate_memoized_effect_dependencies: bool,
    /// Validate that there are no capitalized calls other than the allowlist.
    pub validate_no_capitalized_calls: Option<Vec<String>>,
    /// Validate against impure functions called during render.
    pub validate_no_impure_functions_in_render: bool,
    /// Validate against passing mutable functions to hooks.
    pub validate_no_freezing_known_mutable_functions: bool,
    /// Validate useMemos that don't return any values.
    pub validate_no_void_use_memo: bool,
    /// Validate against blocklisted imports.
    pub validate_blocklisted_imports: Option<Vec<String>>,
    /// Validate that all useMemo/useCallback values are also memoized by the compiler.
    pub validate_preserve_existing_memoization_guarantees: bool,
    /// Enable validation of mutable ranges.
    pub assert_valid_mutable_ranges: bool,

    // --- Feature flags ---
    /// Enable codegen mutability debugging (emits `makeReadOnly` calls).
    pub enable_emit_freeze: bool,
    /// Enable emitting hook guards.
    pub enable_emit_hook_guards: bool,
    /// Enable instruction reordering.
    pub enable_instruction_reordering: bool,
    /// Enable function outlining (extract anonymous functions that don't close over locals).
    pub enable_function_outlining: bool,
    /// Enable JSX outlining (outline nested JSX into separate components).
    pub enable_jsx_outlining: bool,
    /// Enable instrumentation codegen.
    pub enable_emit_instrument_forget: bool,
    /// Enable emitting "change variables" for reactive scope dependencies.
    pub enable_change_variable_codegen: bool,
    /// Enable emitting comments that explain compiler output.
    pub enable_memoization_comments: bool,
    /// Enable the `useFire` transform.
    pub enable_fire: bool,
    /// Enable naming anonymous functions.
    pub enable_name_anonymous_functions: bool,
    /// Enable using existing useMemo/useCallback information to guide memoization.
    pub enable_preserve_existing_memoization_guarantees: bool,
    /// When true, the compiler will not prune existing useMemo/useCallback calls.
    pub enable_preserve_existing_manual_use_memo: bool,
    /// Enable trusting user-supplied type annotations.
    pub enable_use_type_annotations: bool,
    /// Enable inference of optional dependency chains.
    pub enable_optional_dependencies: bool,
    /// Assume hooks follow the Rules of React.
    pub enable_assume_hooks_follow_rules_of_react: bool,
    /// Assume values captured by functions passed to React are not subsequently modified.
    pub enable_transitively_freeze_function_expressions: bool,
    /// Treat deps of function expressions as conditional.
    pub enable_treat_function_deps_as_conditional: bool,
    /// Treat identifiers named `ref` or ending in `Ref` with a `current` property as React refs.
    pub enable_treat_ref_like_identifiers_as_refs: bool,
    /// Treat identifiers with a "set-" prefix that are called somewhere as state setters.
    pub enable_treat_set_identifiers_as_state_setters: bool,
    /// Enable custom type definitions for react-native reanimated library.
    pub enable_custom_type_definition_for_reanimated: bool,
    /// Allow setState calls in effects when the value is derived from a ref.
    pub enable_allow_set_state_from_refs_in_effects: bool,
    /// Always re-compute values (disables memoization for debugging).
    pub disable_memoization_for_debugging: bool,
    /// Enable the new mutation/aliasing model.
    /// Set via `@enableNewMutationAliasingModel` pragma.
    pub enable_new_mutation_aliasing_model: bool,
    /// Enable propagating dependencies in HIR (vs reactive scope pass).
    /// Set via `@enablePropagateDepsInHIR` pragma.
    pub enable_propagate_deps_in_hir: bool,
    /// Enable reactive scopes in HIR.
    /// Set via `@enableReactiveScopesInHIR` pragma.
    pub enable_reactive_scopes_in_hir: bool,
    /// Enable change detection for debugging (emits `$structuralCheck` calls).
    /// Set via `@enableChangeDetectionForDebugging` pragma.
    pub enable_change_detection_for_debugging: bool,
    /// Enable resetting the memo cache when the source file changes (HMR).
    /// Set via `@enableResetCacheOnSourceFileChanges` pragma.
    pub enable_reset_cache_on_source_file_changes: Option<bool>,
    /// Validate against dynamically creating components or hooks during render.
    /// Set via `@validateNoDynamicallyCreatedComponentsOrHooks` pragma.
    pub validate_no_dynamically_created_components_or_hooks: bool,
    /// Throw unknown exceptions in tests (test-only flag).
    /// Set via `@throwUnknownException__testonly` pragma.
    pub throw_unknown_exception_testonly: bool,

    // --- Inline JSX transform ---
    /// Enable inlining ReactElement object literals in place of JSX.
    pub inline_jsx_transform: Option<InlineJsxTransformConfig>,

    // --- Lower context access ---
    /// If set, lower `useContext` calls to use the specified function.
    pub lower_context_access: Option<LowerContextAccessConfig>,

    // --- Hook pattern ---
    /// Pattern for determining which global values should be treated as hooks.
    pub hook_pattern: Option<String>,

    // --- Infer effect dependencies ---
    /// Configuration for automatic effect dependency inference.
    pub infer_effect_dependencies: Option<Vec<InferEffectDepsConfig>>,

    // --- Custom macros ---
    /// Custom macro identifiers that the compiler should treat specially.
    /// Set via `@customMacros:"name"` or `@customMacros(name)` pragma.
    pub custom_macros: Option<Vec<CustomMacroConfig>>,
}

/// Configuration for inline JSX transform.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineJsxTransformConfig {
    pub element_symbol: String,
    pub global_dev_var: String,
}

/// Configuration for lowering context access.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LowerContextAccessConfig {
    pub module: String,
    pub imported_name: String,
}

/// Configuration for automatic effect dependency inference.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferEffectDepsConfig {
    pub function_module: String,
    pub function_name: String,
    pub autodeps_index: usize,
}

/// A property segment in a custom macro path.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MacroProp {
    /// A named property (e.g., `a` in `idx.a`).
    Name(String),
    /// A wildcard (e.g., `*` in `idx.*.b`).
    Wildcard,
}

/// Configuration for a custom macro.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMacroConfig {
    /// The root identifier name (e.g., `cx`, `idx`).
    pub name: String,
    /// Property path segments (for `idx.a` → `[Name("a")]`, for `idx.*.b` → `[Wildcard, Name("b")]`).
    pub props: Vec<MacroProp>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            // Validation flags
            validate_hooks_usage: true,
            validate_ref_access_during_render: true,
            validate_no_set_state_in_render: true,
            validate_no_set_state_in_effects: false,
            validate_no_derived_computations_in_effects: false,
            validate_no_jsx_in_try_statements: false,
            validate_static_components: false,
            validate_memoized_effect_dependencies: false,
            validate_no_capitalized_calls: None,
            validate_no_impure_functions_in_render: false,
            validate_no_freezing_known_mutable_functions: false,
            validate_no_void_use_memo: false,
            validate_blocklisted_imports: None,
            validate_preserve_existing_memoization_guarantees: true,
            assert_valid_mutable_ranges: false,

            // Feature flags
            enable_emit_freeze: false,
            enable_emit_hook_guards: false,
            enable_instruction_reordering: false,
            enable_function_outlining: true,
            enable_jsx_outlining: false,
            enable_emit_instrument_forget: false,
            enable_change_variable_codegen: false,
            enable_memoization_comments: false,
            enable_fire: false,
            enable_name_anonymous_functions: false,
            enable_preserve_existing_memoization_guarantees: true,
            enable_preserve_existing_manual_use_memo: false,
            enable_use_type_annotations: false,
            enable_optional_dependencies: true,
            enable_assume_hooks_follow_rules_of_react: true,
            enable_transitively_freeze_function_expressions: true,
            enable_treat_function_deps_as_conditional: false,
            enable_treat_ref_like_identifiers_as_refs: true,
            enable_treat_set_identifiers_as_state_setters: false,
            enable_custom_type_definition_for_reanimated: false,
            enable_allow_set_state_from_refs_in_effects: true,
            disable_memoization_for_debugging: false,
            enable_new_mutation_aliasing_model: false,
            enable_propagate_deps_in_hir: false,
            enable_reactive_scopes_in_hir: false,
            enable_change_detection_for_debugging: false,
            enable_reset_cache_on_source_file_changes: None,
            validate_no_dynamically_created_components_or_hooks: false,
            throw_unknown_exception_testonly: false,

            // Complex configs
            inline_jsx_transform: None,
            lower_context_access: None,
            hook_pattern: None,
            infer_effect_dependencies: None,
            custom_macros: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_compilation_mode_is_infer() {
        assert_eq!(CompilationMode::default(), CompilationMode::Infer);
    }

    #[test]
    fn default_panic_threshold_is_none() {
        assert_eq!(PanicThreshold::default(), PanicThreshold::None);
    }

    #[test]
    fn default_plugin_options_compilation_mode() {
        assert_eq!(
            PluginOptions::default().compilation_mode,
            CompilationMode::Infer
        );
    }

    #[test]
    fn default_plugin_options_panic_threshold() {
        assert_eq!(
            PluginOptions::default().panic_threshold,
            PanicThreshold::None
        );
    }

    #[test]
    fn default_environment_config_validate_hooks() {
        assert!(EnvironmentConfig::default().validate_hooks_usage);
    }

    #[test]
    fn default_environment_config_disable_memoization() {
        assert!(!EnvironmentConfig::default().disable_memoization_for_debugging);
    }

    #[test]
    fn compilation_mode_all_variant() {
        assert_ne!(CompilationMode::All, CompilationMode::default());
    }

    #[test]
    fn environment_config_custom_macro() {
        let config = EnvironmentConfig {
            custom_macros: Some(vec![CustomMacroConfig {
                name: "cx".to_string(),
                props: vec![MacroProp::Name("foo".to_string())],
            }]),
            ..EnvironmentConfig::default()
        };
        assert!(config.custom_macros.is_some());
    }
}
