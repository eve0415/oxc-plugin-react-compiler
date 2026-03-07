use oxc_ast::ast;

use crate::CompileResult;
use crate::options::PluginOptions;

pub(crate) mod ast_backend;
pub(crate) mod raw;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodegenBackend {
    Raw,
    Ast,
    Compare,
}

impl CodegenBackend {
    pub(crate) fn from_env() -> Self {
        match std::env::var("OXC_REACT_CODEGEN_BACKEND").ok().as_deref() {
            Some("ast") => Self::Ast,
            Some("compare") => Self::Compare,
            _ => Self::Raw,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CompiledFunction {
    pub(crate) name: String,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) generated_body: String,
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
    pub(crate) needs_instrument_forget: bool,
    pub(crate) needs_emit_freeze: bool,
    pub(crate) outlined_functions: Vec<(String, String, String)>,
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
    pub(crate) program: &'a ast::Program<'a>,
    pub(crate) options: &'a PluginOptions,
    pub(crate) dynamic_gate_ident: Option<&'a str>,
}

pub(crate) fn emit_module(
    backend: CodegenBackend,
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    match backend {
        CodegenBackend::Raw => raw::emit_module(args, compiled),
        CodegenBackend::Ast => ast_backend::emit_module(args, compiled),
        CodegenBackend::Compare => {
            let raw_result = raw::emit_module(args, compiled.clone());
            let ast_result = ast_backend::emit_module(args, compiled);
            if normalize_backend_compare(&raw_result.code)
                != normalize_backend_compare(&ast_result.code)
            {
                eprintln!(
                    "[OXC_REACT_CODEGEN_BACKEND=compare] raw and ast outputs differ for {}",
                    args.filename
                );
            }
            raw_result
        }
    }
}

fn normalize_backend_compare(code: &str) -> String {
    code.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
