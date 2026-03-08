use oxc_ast::ast;
use oxc_span::SourceType;

use crate::CompileResult;
use crate::options::PluginOptions;

pub(crate) mod ast_backend;
pub(crate) mod hir_to_ast;
pub(crate) mod raw;
pub(crate) mod shared;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompiledBodyPayload {
    GeneratedString,
    LowerFromFinalHir,
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledFunction {
    pub(crate) name: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) generated_body: String,
    pub(crate) body_payload: CompiledBodyPayload,
    pub(crate) needs_cache_import: bool,
    pub(crate) params_str: String,
    pub(crate) original_params_str: String,
    pub(crate) param_destructurings: Vec<String>,
    pub(crate) is_async: bool,
    pub(crate) is_generator: bool,
    pub(crate) is_arrow: bool,
    pub(crate) is_function_declaration: bool,
    pub(crate) body_start: u32,
    pub(crate) body_end: u32,
    pub(crate) directives: Vec<String>,
    pub(crate) preserved_body_statements: Vec<String>,
    pub(crate) hir_function: Option<crate::hir::types::HIRFunction>,
    pub(crate) needs_instrument_forget: bool,
    pub(crate) needs_emit_freeze: bool,
    pub(crate) outlined_functions: Vec<(String, String, String)>,
    pub(crate) hir_outlined_functions: Vec<(String, crate::hir::types::HIRFunction)>,
    pub(crate) has_fire_rewrite: bool,
    pub(crate) needs_hook_guards: bool,
    pub(crate) needs_structural_check_import: bool,
    pub(crate) needs_lower_context_access: bool,
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
    ast_backend::emit_module(args, compiled)
}
