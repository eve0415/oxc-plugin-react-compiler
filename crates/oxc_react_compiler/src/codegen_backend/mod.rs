use oxc_ast::ast;
use oxc_span::SourceType;

use crate::CompileResult;
use crate::options::PluginOptions;

pub(crate) mod hir_to_ast;
pub(crate) mod module_emitter;
pub(crate) mod shared;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledParam {
    pub(crate) name: String,
    pub(crate) is_rest: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompiledBindingPattern {
    Identifier(String),
    Object(CompiledObjectPattern),
    Array(CompiledArrayPattern),
    Assignment {
        left: Box<CompiledBindingPattern>,
        default_expr: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledObjectPattern {
    pub(crate) properties: Vec<CompiledObjectPatternProperty>,
    pub(crate) rest: Option<Box<CompiledBindingPattern>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledObjectPatternProperty {
    pub(crate) key: CompiledPropertyKey,
    pub(crate) value: CompiledBindingPattern,
    pub(crate) shorthand: bool,
    pub(crate) computed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledArrayPattern {
    pub(crate) elements: Vec<Option<CompiledBindingPattern>>,
    pub(crate) rest: Option<Box<CompiledBindingPattern>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompiledPropertyKey {
    StaticIdentifier(String),
    StringLiteral(String),
    Source(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompiledInitializer {
    Identifier(String),
    UndefinedFallback {
        temp_name: String,
        default_expr: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompiledParamPrefixStatement {
    pub(crate) kind: ast::VariableDeclarationKind,
    pub(crate) pattern: CompiledBindingPattern,
    pub(crate) init: CompiledInitializer,
}

#[derive(Debug)]
pub(crate) struct CompiledOutlinedFunction {
    pub(crate) name: String,
    pub(crate) params: Vec<CompiledParam>,
    pub(crate) directives: Vec<String>,
    pub(crate) cache_prologue: Option<crate::reactive_scopes::codegen_ast::CachePrologue>,
    pub(crate) needs_function_hook_guard_wrapper: bool,
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    #[allow(dead_code)]
    pub(crate) reactive_function: Option<crate::hir::types::ReactiveFunction>,
    /// The outlined function's own unique identifiers from its rename_variables pass.
    /// Used instead of the parent function's unique_identifiers to ensure independent naming.
    pub(crate) unique_identifiers: std::collections::HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SynthesizedDefaultParamCache {
    pub(crate) value_name: String,
    pub(crate) temp_name: String,
    pub(crate) value_expr: String,
}

#[derive(Debug)]
pub(crate) struct CompiledFunction {
    pub(crate) name: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) reactive_function: Option<crate::hir::types::ReactiveFunction>,
    pub(crate) needs_cache_import: bool,
    pub(crate) compiled_params: Option<Vec<CompiledParam>>,
    pub(crate) param_prefix_statements: Vec<CompiledParamPrefixStatement>,
    pub(crate) synthesized_default_param_cache: Option<SynthesizedDefaultParamCache>,
    pub(crate) is_function_declaration: bool,
    pub(crate) directives: Vec<String>,
    pub(crate) hir_function: Option<crate::hir::types::HIRFunction>,
    pub(crate) cache_prologue: Option<crate::reactive_scopes::codegen_ast::CachePrologue>,
    pub(crate) needs_function_hook_guard_wrapper: bool,
    pub(crate) normalize_use_fire_binding_temps: bool,
    pub(crate) needs_instrument_forget: bool,
    pub(crate) needs_emit_freeze: bool,
    pub(crate) outlined_functions: Vec<CompiledOutlinedFunction>,
    pub(crate) hir_outlined_functions: Vec<(String, crate::hir::types::HIRFunction)>,
    pub(crate) has_fire_rewrite: bool,
    pub(crate) needs_hook_guards: bool,
    pub(crate) needs_structural_check_import: bool,
    pub(crate) needs_lower_context_access: bool,
    // --- AST codegen pipeline options ---
    pub(crate) enable_change_variable_codegen: bool,
    pub(crate) enable_emit_hook_guards: bool,
    pub(crate) enable_change_detection_for_debugging: bool,
    pub(crate) enable_reset_cache_on_source_file_changes: bool,
    pub(crate) fast_refresh_source_hash: Option<String>,
    pub(crate) disable_memoization_features: bool,
    pub(crate) disable_memoization_for_debugging: bool,
    pub(crate) fbt_operands: std::collections::HashSet<crate::hir::types::IdentifierId>,
    pub(crate) unique_identifiers: std::collections::HashSet<String>,
    pub(crate) enable_name_anonymous_functions: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct ModuleEmitArgs<'a> {
    pub(crate) filename: &'a str,
    pub(crate) source: &'a str,
    pub(crate) source_untransformed: &'a str,
    pub(crate) source_type: SourceType,
    pub(crate) program: &'a ast::Program<'a>,
    pub(crate) options: &'a PluginOptions,
    pub(crate) dynamic_gate_ident: Option<&'a str>,
}

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    module_emitter::emit_module(args, compiled)
}
