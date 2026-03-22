use oxc_react_compiler::options::{CompilationMode, PanicThreshold};

/// Parsed pragma information from a fixture's first line.
pub(crate) struct Pragma {
    pub(crate) compilation_mode: CompilationMode,
    pub(crate) panic_threshold: PanicThreshold,
    pub(crate) should_skip: bool,
    pub(crate) custom_opt_out_directives: Vec<String>,
    pub(crate) ignore_use_no_forget: bool,

    // --- PluginOptions-level pragmas ---
    pub(crate) gating: bool,
    pub(crate) dynamic_gating: Option<String>, // source module
    pub(crate) no_emit: bool,
    pub(crate) target: Option<String>,
    pub(crate) eslint_suppression_rules: Option<Vec<String>>,
    pub(crate) flow_suppressions: Option<bool>,
    pub(crate) logger_test_only: bool,

    // --- EnvironmentConfig boolean flags ---
    /// Each `Option<bool>` is `None` if not specified, `Some(true/false)` if explicitly set.
    pub(crate) validate_preserve_existing_memoization_guarantees: Option<bool>,
    pub(crate) validate_ref_access_during_render: Option<bool>,
    pub(crate) validate_no_set_state_in_render: Option<bool>,
    pub(crate) validate_no_set_state_in_effects: Option<bool>,
    pub(crate) validate_no_derived_computations_in_effects: Option<bool>,
    pub(crate) validate_no_jsx_in_try_statements: Option<bool>,
    pub(crate) validate_static_components: Option<bool>,
    pub(crate) validate_memoized_effect_dependencies: Option<bool>,
    pub(crate) validate_no_capitalized_calls: Option<bool>,
    pub(crate) validate_no_impure_functions_in_render: Option<bool>,
    pub(crate) validate_no_freezing_known_mutable_functions: Option<bool>,
    pub(crate) validate_no_void_use_memo: Option<bool>,
    pub(crate) validate_blocklisted_imports: Option<Vec<String>>,
    pub(crate) validate_no_dynamically_created_components_or_hooks: Option<bool>,

    pub(crate) enable_preserve_existing_memoization_guarantees: Option<bool>,
    pub(crate) enable_transitively_freeze_function_expressions: Option<bool>,
    pub(crate) enable_assume_hooks_follow_rules_of_react: Option<bool>,
    pub(crate) enable_optional_dependencies: Option<bool>,
    pub(crate) enable_treat_function_deps_as_conditional: Option<bool>,
    pub(crate) enable_treat_ref_like_identifiers_as_refs: Option<bool>,
    pub(crate) enable_treat_set_identifiers_as_state_setters: Option<bool>,
    pub(crate) enable_use_type_annotations: Option<bool>,
    pub(crate) enable_jsx_outlining: Option<bool>,
    pub(crate) enable_instruction_reordering: Option<bool>,
    pub(crate) enable_memoization_comments: Option<bool>,
    pub(crate) enable_name_anonymous_functions: Option<bool>,
    pub(crate) enable_custom_type_definition_for_reanimated: Option<bool>,
    pub(crate) enable_allow_set_state_from_refs_in_effects: Option<bool>,
    pub(crate) disable_memoization_for_debugging: Option<bool>,
    pub(crate) enable_preserve_existing_manual_use_memo: Option<bool>,
    pub(crate) enable_new_mutation_aliasing_model: Option<bool>,
    pub(crate) enable_propagate_deps_in_hir: Option<bool>,
    pub(crate) enable_reactive_scopes_in_hir: Option<bool>,
    pub(crate) enable_change_detection_for_debugging: Option<bool>,
    pub(crate) enable_reset_cache_on_source_file_changes: Option<bool>,
    pub(crate) throw_unknown_exception_testonly: Option<bool>,

    // --- Complex EnvironmentConfig pragmas ---
    pub(crate) enable_emit_freeze: Option<bool>,
    pub(crate) enable_emit_hook_guards: Option<bool>,
    pub(crate) enable_emit_instrument_forget: Option<bool>,
    pub(crate) enable_change_variable_codegen: Option<bool>,
    pub(crate) enable_fire: Option<bool>,
    pub(crate) inline_jsx_transform: Option<bool>,
    pub(crate) instrument_forget: Option<bool>,
    pub(crate) lower_context_access: bool,
    pub(crate) infer_effect_dependencies: bool,
    pub(crate) hook_pattern: Option<String>,
    pub(crate) custom_macros: Option<String>,
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
pub(crate) fn parse_pragma(source: &str) -> Pragma {
    // Pragmas can appear on the first line (even after code, e.g. `import ... // @flag`)
    // and on subsequent leading comment-only lines (e.g. `// @flag` on line 2+).
    let first_line = source.lines().next().unwrap_or("");
    let additional_pragma_lines: Vec<&str> = source
        .lines()
        .skip(1)
        .take_while(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("//") || trimmed.is_empty()
        })
        .filter(|line| line.trim().starts_with("//") && line.contains('@'))
        .collect();

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
    // Parse pragmas from first line + additional leading comment lines
    let mut entries = split_pragma(first_line);
    for pragma_line in &additional_pragma_lines {
        entries.extend(split_pragma(pragma_line));
    }
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
