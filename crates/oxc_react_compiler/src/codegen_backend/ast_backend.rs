use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn, Dummy};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_ast_visit::{Visit, VisitMut, walk, walk_mut};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};
use oxc_syntax::identifier::is_identifier_name;
use oxc_syntax::number::NumberBase;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator, LogicalOperator};

use crate::CompileResult;

use super::{
    CompiledArrayPattern, CompiledBindingPattern, CompiledBodyPayload, CompiledFunction,
    CompiledInitializer, CompiledObjectPattern, CompiledParam, CompiledParamPrefixStatement,
    CompiledPropertyKey, ModuleEmitArgs, SynthesizedDefaultParamCache,
};

struct AstRenderState {
    source_type: SourceType,
    cache_import_name: String,
    make_read_only_ident: String,
    should_instrument_ident: String,
    use_render_counter_ident: String,
    hook_guard_ident: String,
    structural_check_ident: String,
    lower_context_access_ident: String,
    lower_context_access_imported: String,
    gating_local_name: Option<String>,
    imports_to_insert: Vec<InsertedImport>,
    runtime_import_merge_plan: Option<crate::pipeline::RuntimeImportMergePlan>,
    instrument_source_path: String,
}

struct InsertedImport {
    source: String,
    specs: Vec<InsertedImportSpec>,
    is_script: bool,
}

struct InsertedImportSpec {
    imported: String,
    local: String,
}

const FLOW_CAST_MARKER_HELPER: &str = "__REACT_COMPILER_FLOW_CAST__";

struct RenderedOutlinedFunction {
    name: String,
    params: Vec<CompiledParam>,
    body: Option<String>,
    body_shape: crate::reactive_scopes::codegen_reactive::GeneratedBodyShape,
    directives: Vec<String>,
    cache_prologue: Option<crate::reactive_scopes::codegen_reactive::CachePrologue>,
    needs_function_hook_guard_wrapper: bool,
    is_async: bool,
    is_generator: bool,
}

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    if compiled.is_empty() {
        return CompileResult {
            transformed: false,
            code: args.source_untransformed.to_string(),
            map: None,
        };
    }

    match try_emit_module(args, &compiled) {
        Ok(mut result) => {
            result.transformed =
                compute_transform_state(args.source_type, &result.code, args.source_untransformed);
            result
        }
        Err(_) => CompileResult {
            transformed: false,
            code: args.source_untransformed.to_string(),
            map: None,
        },
    }
}

fn compute_transform_state(
    source_type: SourceType,
    output_code: &str,
    source_untransformed: &str,
) -> bool {
    let output = normalize_module_for_transform_flag(source_type, output_code);
    let source = normalize_module_for_transform_flag(source_type, source_untransformed);
    if output.normalized == source.normalized {
        return false;
    }
    if let (Some(output_canonical), Some(source_canonical)) = (&output.canonical, &source.canonical)
        && output_canonical == source_canonical
    {
        return false;
    }
    match (output.canonical, source.canonical) {
        (Some(output_canonical), Some(source_canonical)) => output_canonical != source_canonical,
        _ => true,
    }
}

struct TransformFlagNormalization {
    normalized: String,
    canonical: Option<String>,
}

fn normalize_module_for_transform_flag(
    source_type: SourceType,
    code: &str,
) -> TransformFlagNormalization {
    let stripped = strip_nonsemantic_top_level_comments_for_transform_flag(source_type, code)
        .unwrap_or_else(|| StrippedTransformFlagCode {
            code: strip_leading_comments_for_transform_flag(code).to_string(),
            has_nested_comments: code.contains("//") || code.contains("/*"),
        });
    let flow_marker_rewritten = rewrite_flow_cast_marker_calls(
        &crate::pipeline::rewrite_flow_cast_expressions(&stripped.code),
    );
    let normalized = super::shared::normalize_for_transform_flag(&flow_marker_rewritten);
    let canonical = if stripped.has_nested_comments {
        None
    } else {
        canonicalize_module_for_transform_flag(source_type, &flow_marker_rewritten)
    };
    TransformFlagNormalization {
        normalized,
        canonical,
    }
}

struct StrippedTransformFlagCode {
    code: String,
    has_nested_comments: bool,
}

fn strip_nonsemantic_top_level_comments_for_transform_flag(
    source_type: SourceType,
    code: &str,
) -> Option<StrippedTransformFlagCode> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return None;
    }

    let statements = parsed
        .program
        .body
        .iter()
        .map(GetSpan::span)
        .collect::<Vec<_>>();
    let comments = parsed.program.comments.iter().collect::<Vec<_>>();
    let mut has_nested_comments = false;
    let mut stripped = String::with_capacity(code.len());
    let mut cursor = 0usize;

    for comment in comments {
        let comment_start = comment.span.start as usize;
        let comment_end = comment.span.end as usize;
        let is_nested = statements
            .iter()
            .any(|span| comment.span.start >= span.start && comment.span.end <= span.end);

        if is_nested {
            has_nested_comments = true;
            continue;
        }

        if cursor < comment_start {
            stripped.push_str(&code[cursor..comment_start]);
        }
        cursor = comment_end;
    }

    if cursor < code.len() {
        stripped.push_str(&code[cursor..]);
    }

    Some(StrippedTransformFlagCode {
        code: strip_leading_comments_for_transform_flag(&stripped).to_string(),
        has_nested_comments,
    })
}

fn canonicalize_module_for_transform_flag(source_type: SourceType, code: &str) -> Option<String> {
    try_canonicalize_module(source_type, code)
}

fn try_canonicalize_module(source_type: SourceType, code: &str) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return None;
    }
    Some(codegen_program(&parsed.program))
}

fn strip_leading_comments_for_transform_flag(code: &str) -> &str {
    let bytes = code.as_bytes();
    let mut i = 0usize;
    let len = bytes.len();
    loop {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 >= len {
            return &code[i..];
        }
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            } else {
                return "";
            }
            continue;
        }
        return &code[i..];
    }
}

fn try_emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: &[CompiledFunction],
) -> Result<CompileResult, String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let state = build_render_state(args, compiled);
    let gate_name = state
        .gating_local_name
        .as_deref()
        .filter(|_| compiled.iter().any(|cf| cf.needs_cache_import));

    let mut body = builder.vec();
    for import_plan in &state.imports_to_insert {
        let statement = build_inserted_import_statement(builder, import_plan);
        body.push(statement);
    }

    let mut compiled_sorted = compiled.iter().collect::<Vec<_>>();
    compiled_sorted.sort_by_key(|cf| cf.start);
    let mut compiled_idx = 0usize;

    for stmt in &args.program.body {
        if let ast::Statement::ImportDeclaration(import_decl) = stmt
            && let Some(plan) = state.runtime_import_merge_plan.as_ref()
            && import_decl.span.start == plan.start
            && import_decl.span.end == plan.end
        {
            if plan.replacement.is_some() {
                let statement = build_runtime_import_merge_statement(builder, &plan.merged_specs);
                body.push(statement);
            } else {
                body.push(stmt.clone_in(&allocator));
            }
            continue;
        }

        let span = stmt.span();
        while compiled_idx < compiled_sorted.len()
            && compiled_sorted[compiled_idx].end <= span.start
        {
            compiled_idx += 1;
        }

        let mut stmt_compiled = Vec::new();
        let mut lookahead = compiled_idx;
        while lookahead < compiled_sorted.len() {
            let cf = compiled_sorted[lookahead];
            if cf.start >= span.end {
                break;
            }
            if cf.end > span.end {
                return Err(format!(
                    "compiled function span [{}..{}] escaped statement span [{}..{}]",
                    cf.start, cf.end, span.start, span.end
                ));
            }
            stmt_compiled.push(cf);
            lookahead += 1;
        }
        compiled_idx = lookahead;

        if stmt_compiled.is_empty() {
            let original_stmt = args.source[span.start as usize..span.end as usize].to_string();
            let maybe_gated = gate_name
                .map(|name| {
                    maybe_gate_entrypoint_source(state.source_type, original_stmt.clone(), name)
                })
                .unwrap_or(original_stmt.clone());
            if maybe_gated == original_stmt {
                body.push(stmt.clone_in(&allocator));
            } else {
                body.extend(parse_statements(
                    &allocator,
                    state.source_type,
                    allocator.alloc_str(&maybe_gated),
                )?);
            }
            continue;
        }

        if stmt_compiled.len() == 1
            && stmt_compiled[0].start == span.start
            && stmt_compiled[0].end == span.end
            && let Some(statements) = try_lower_compiled_statement_ast(
                builder,
                &allocator,
                state.source_type,
                stmt_compiled[0],
                &state,
            )
        {
            for statement in statements {
                body.push(statement);
            }
            continue;
        }

        if let Some(statements) = try_rewrite_compiled_statement_ast(
            builder,
            &allocator,
            state.source_type,
            args.source,
            stmt,
            &stmt_compiled,
            &state,
        ) {
            for statement in statements {
                body.push(statement);
            }
            continue;
        }

        return Err(format!(
            "unable to AST-rewrite compiled statement [{}..{}] in {}",
            span.start, span.end, args.filename
        ));
    }

    let program = builder.program(
        SPAN,
        state.source_type,
        "",
        builder.vec(),
        args.program.hashbang.clone_in(&allocator),
        args.program.directives.clone_in(&allocator),
        body,
    );
    let mut code = codegen_program(&program);
    if code.contains(FLOW_CAST_MARKER_HELPER) {
        code = restore_flow_cast_marker_calls(&code);
    }
    Ok(CompileResult {
        transformed: true,
        code,
        map: None,
    })
}

fn build_render_state(args: ModuleEmitArgs<'_>, compiled: &[CompiledFunction]) -> AstRenderState {
    let needs_cache_import = compiled.iter().any(|c| c.needs_cache_import);
    let needs_fire_import = compiled.iter().any(|c| c.has_fire_rewrite);
    let top_level_bindings = crate::pipeline::collect_top_level_bindings(args.program);
    let is_script = args.source.contains("// @script") || args.source.contains("/* @script");

    let mut all_bindings = crate::pipeline::collect_all_program_bindings(args.program);
    let mut cache_import_name = crate::pipeline::generate_unique_name("_c", &all_bindings);
    let runtime_import_merge_plan = if !is_script && (needs_cache_import || needs_fire_import) {
        crate::pipeline::plan_runtime_import_merge(
            args.program,
            needs_cache_import,
            needs_fire_import,
            &cache_import_name,
        )
    } else {
        None
    };
    if needs_cache_import {
        if let Some(existing_cache_local) = runtime_import_merge_plan
            .as_ref()
            .filter(|plan| plan.has_cache_after)
            .and_then(|plan| plan.cache_local_name.as_ref())
        {
            cache_import_name = existing_cache_local.clone();
        }
        all_bindings.insert(cache_import_name.clone());
    }

    let needs_freeze_import =
        args.options.environment.enable_emit_freeze && compiled.iter().any(|c| c.needs_emit_freeze);
    let mut make_read_only_ident = String::new();
    if needs_freeze_import {
        make_read_only_ident =
            crate::pipeline::generate_unique_import_binding("makeReadOnly", &all_bindings);
        all_bindings.insert(make_read_only_ident.clone());
    }

    let needs_instrument_import = args.options.environment.enable_emit_instrument_forget
        && compiled.iter().any(|c| c.needs_instrument_forget);
    let mut should_instrument_ident = String::new();
    let mut use_render_counter_ident = String::new();
    if needs_instrument_import {
        should_instrument_ident =
            crate::pipeline::generate_unique_import_binding("shouldInstrument", &all_bindings);
        all_bindings.insert(should_instrument_ident.clone());
        use_render_counter_ident =
            crate::pipeline::generate_unique_import_binding("useRenderCounter", &all_bindings);
        all_bindings.insert(use_render_counter_ident.clone());
    }

    let needs_hook_guard_import = args.options.environment.enable_emit_hook_guards
        && compiled.iter().any(|c| c.needs_hook_guards);
    let mut hook_guard_ident = String::new();
    if needs_hook_guard_import {
        hook_guard_ident =
            crate::pipeline::generate_unique_import_binding("$dispatcherGuard", &all_bindings);
        all_bindings.insert(hook_guard_ident.clone());
    }

    let needs_structural_check_import = args
        .options
        .environment
        .enable_change_detection_for_debugging
        && compiled.iter().any(|c| c.needs_structural_check_import);
    let mut structural_check_ident = String::new();
    if needs_structural_check_import {
        structural_check_ident =
            crate::pipeline::generate_unique_import_binding("$structuralCheck", &all_bindings);
        all_bindings.insert(structural_check_ident.clone());
    }

    let lower_context_access_config = args.options.environment.lower_context_access.as_ref();
    let needs_lower_context_access_import = lower_context_access_config.is_some()
        && compiled.iter().any(|c| c.needs_lower_context_access);
    let mut lower_context_access_ident = String::new();
    let mut lower_context_access_imported = String::new();
    let mut lower_context_access_module = String::new();
    if let Some(config) = lower_context_access_config.filter(|_| needs_lower_context_access_import)
    {
        lower_context_access_ident =
            crate::pipeline::generate_unique_import_binding(&config.imported_name, &all_bindings);
        all_bindings.insert(lower_context_access_ident.clone());
        lower_context_access_imported = config.imported_name.clone();
        lower_context_access_module = config
            .module
            .trim()
            .trim_matches(|c: char| {
                c.is_whitespace() || c == '"' || c == '\'' || c == '{' || c == '}'
            })
            .to_string();
    }

    let instrument_source_path = format!(
        "/{}.ts",
        args.filename
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(args.filename)
    );

    let mut gating_local_name = None;
    let mut imports_to_insert = Vec::new();
    let mut runtime_support_specs: Vec<InsertedImportSpec> = Vec::new();
    if needs_freeze_import {
        runtime_support_specs.push(InsertedImportSpec {
            imported: "makeReadOnly".to_string(),
            local: make_read_only_ident.clone(),
        });
    }
    if needs_instrument_import {
        runtime_support_specs.push(InsertedImportSpec {
            imported: "shouldInstrument".to_string(),
            local: should_instrument_ident.clone(),
        });
        runtime_support_specs.push(InsertedImportSpec {
            imported: "useRenderCounter".to_string(),
            local: use_render_counter_ident.clone(),
        });
    }
    if needs_hook_guard_import {
        runtime_support_specs.push(InsertedImportSpec {
            imported: "$dispatcherGuard".to_string(),
            local: hook_guard_ident.clone(),
        });
    }
    if needs_structural_check_import {
        runtime_support_specs.push(InsertedImportSpec {
            imported: "$structuralCheck".to_string(),
            local: structural_check_ident.clone(),
        });
    }
    if needs_lower_context_access_import && lower_context_access_module == "react-compiler-runtime"
    {
        runtime_support_specs.push(InsertedImportSpec {
            imported: lower_context_access_imported.clone(),
            local: lower_context_access_ident.clone(),
        });
    }
    if !runtime_support_specs.is_empty() {
        imports_to_insert.push(InsertedImport {
            source: "react-compiler-runtime".to_string(),
            specs: runtime_support_specs,
            is_script,
        });
    }

    let runtime_import_covered_by_existing =
        runtime_import_merge_plan.as_ref().is_some_and(|plan| {
            (!needs_cache_import || plan.has_cache_after)
                && (!needs_fire_import || plan.has_use_fire_after)
        });
    if (needs_cache_import || needs_fire_import) && !runtime_import_covered_by_existing {
        let mut runtime_import_specs = Vec::new();
        if needs_cache_import {
            runtime_import_specs.push(InsertedImportSpec {
                imported: "c".to_string(),
                local: cache_import_name.clone(),
            });
        }
        if needs_fire_import {
            runtime_import_specs.push(InsertedImportSpec {
                imported: "useFire".to_string(),
                local: "useFire".to_string(),
            });
        }
        imports_to_insert.push(InsertedImport {
            source: "react/compiler-runtime".to_string(),
            specs: runtime_import_specs,
            is_script,
        });
    }

    if needs_lower_context_access_import
        && !lower_context_access_module.is_empty()
        && lower_context_access_module != "react-compiler-runtime"
    {
        imports_to_insert.push(InsertedImport {
            source: lower_context_access_module.clone(),
            specs: vec![InsertedImportSpec {
                imported: lower_context_access_imported.clone(),
                local: lower_context_access_ident.clone(),
            }],
            is_script,
        });
    }

    if let Some((source_mod, base)) = args
        .options
        .gating
        .as_ref()
        .map(|g| (g.source.as_str(), g.import_specifier_name.as_str()))
        .or_else(|| {
            args.dynamic_gate_ident
                .zip(
                    args.options
                        .dynamic_gating
                        .as_ref()
                        .map(|g| g.source.as_str()),
                )
                .map(|(ident, source)| (source, ident))
        })
    {
        let source_mod = source_mod.trim().trim_matches(|c: char| {
            c.is_whitespace() || c == '"' || c == '\'' || c == '{' || c == '}'
        });
        let local = if top_level_bindings.contains(base) {
            format!("_{}", base)
        } else {
            base.to_string()
        };
        gating_local_name = Some(local.clone());
        if needs_cache_import {
            imports_to_insert.push(InsertedImport {
                source: source_mod.to_string(),
                specs: vec![InsertedImportSpec {
                    imported: base.to_string(),
                    local,
                }],
                is_script,
            });
        }
    }

    AstRenderState {
        source_type: args.source_type,
        cache_import_name,
        make_read_only_ident,
        should_instrument_ident,
        use_render_counter_ident,
        hook_guard_ident,
        structural_check_ident,
        lower_context_access_ident,
        lower_context_access_imported,
        gating_local_name,
        imports_to_insert,
        runtime_import_merge_plan,
        instrument_source_path,
    }
}

fn try_rewrite_compiled_statement_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    source: &str,
    stmt: &ast::Statement<'_>,
    compiled: &[&CompiledFunction],
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let mut rewritten_stmt = stmt.clone_in(allocator);
    let stmt_source = &source[stmt.span().start as usize..stmt.span().end as usize];
    if compiled.len() == 1
        && let Some(gate_name) = state
            .gating_local_name
            .as_deref()
            .filter(|_| compiled[0].needs_cache_import)
        && let Some(statements) = try_build_gated_function_declaration_statements(
            builder,
            allocator,
            source,
            stmt,
            gate_name,
            compiled[0],
            state,
        )
    {
        return Some(statements);
    }
    let mut outlined_functions = Vec::new();
    for cf in compiled {
        if state.gating_local_name.is_some()
            && cf.needs_cache_import
            && stmt_source.contains("FIXTURE_ENTRYPOINT")
        {
            return None;
        }

        let function_body = build_compiled_function_body(
            builder,
            allocator,
            source_type,
            cf,
            state,
            find_original_compiled_function_body(stmt, cf),
        )?;
        let compiled_params = cf.compiled_params.as_deref()?;
        let rewritten = if let Some(gate_name) = state
            .gating_local_name
            .as_deref()
            .filter(|_| cf.needs_cache_import)
        {
            replace_compiled_function_in_statement_with_gate(
                builder,
                allocator,
                &mut rewritten_stmt,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )
        } else {
            replace_compiled_function_in_statement(
                builder,
                allocator,
                &mut rewritten_stmt,
                cf,
                compiled_params,
                &function_body,
            )
        };
        if !rewritten {
            return None;
        }

        outlined_functions.extend(collect_rendered_outlined_functions(cf));
    }

    let mut statements = builder.vec1(rewritten_stmt);
    let mut emitted_hir_outlined_names = HashSet::new();
    for outlined in outlined_functions {
        emitted_hir_outlined_names.insert(outlined.name.clone());
        statements.push(build_rendered_outlined_function_statement(
            builder,
            allocator,
            source_type,
            &outlined,
            state,
        )?);
    }
    for cf in compiled {
        for (name, hir_function) in &cf.hir_outlined_functions {
            if !emitted_hir_outlined_names.insert(name.clone()) {
                continue;
            }
            statements.push(super::hir_to_ast::try_lower_function_declaration_ast(
                builder,
                hir_function,
            )?);
        }
    }

    Some(statements)
}

fn try_build_gated_function_declaration_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source: &str,
    stmt: &ast::Statement<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if cf.name.is_empty() {
        return None;
    }
    if source[stmt.span().start as usize..stmt.span().end as usize].contains("FIXTURE_ENTRYPOINT") {
        return None;
    }
    let referenced_before_decl = crate::pipeline::has_early_binding_reference(
        &source[..stmt.span().start as usize],
        &cf.name,
    );

    let function_body = build_compiled_function_body(
        builder,
        allocator,
        state.source_type,
        cf,
        state,
        find_original_compiled_function_body(stmt, cf),
    )?;
    let compiled_params = cf.compiled_params.as_deref()?;

    match stmt {
        ast::Statement::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            if referenced_before_decl {
                return build_early_reference_gated_function_declaration_statements(
                    builder,
                    allocator,
                    function,
                    gate_name,
                    cf,
                    compiled_params,
                    &function_body,
                );
            }
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            Some(builder.vec1(build_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            )))
        }
        ast::Statement::ExportNamedDeclaration(export_named)
            if matches!(
                export_named.declaration.as_ref(),
                Some(ast::Declaration::FunctionDeclaration(function))
                    if function.span.start == cf.start && function.span.end == cf.end
            ) =>
        {
            let ast::Declaration::FunctionDeclaration(function) =
                export_named.declaration.as_ref().unwrap()
            else {
                unreachable!();
            };
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            Some(builder.vec1(build_exported_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            )))
        }
        ast::Statement::ExportDefaultDeclaration(export_default)
            if matches!(
                &export_default.declaration,
                ast::ExportDefaultDeclarationKind::FunctionDeclaration(function)
                    if function.span.start == cf.start && function.span.end == cf.end
            ) =>
        {
            let ast::ExportDefaultDeclarationKind::FunctionDeclaration(function) =
                &export_default.declaration
            else {
                unreachable!();
            };
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            let mut statements = builder.vec();
            statements.push(build_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            ));
            statements.push(build_export_default_identifier_statement(
                builder,
                stmt.span(),
                &cf.name,
            ));
            Some(statements)
        }
        _ => None,
    }
}

fn build_gated_function_declaration_initializer<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> Option<ast::Expression<'a>> {
    let original = function_declaration_to_expression(builder, allocator, function);
    let mut optimized = original.clone_in(allocator);
    if !replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut optimized,
        cf,
        compiled_params,
        function_body,
    ) {
        return None;
    }
    Some(make_gate_conditional_expression(
        builder,
        gate_name,
        function.span,
        optimized,
        original,
    ))
}

fn build_early_reference_gated_function_declaration_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let gate_result_name = format!("{}_result", gate_name);
    let optimized_name = format!("{}_optimized", cf.name);
    let unoptimized_name = format!("{}_unoptimized", cf.name);
    let param_count = compiled_params.len();
    let wrapper_args = (0..param_count)
        .map(|i| format!("arg{i}"))
        .collect::<Vec<_>>();

    let mut statements = builder.vec();
    statements.push(build_const_binding_statement(
        builder,
        function.span,
        &gate_result_name,
        build_identifier_call_expression(builder, function.span, gate_name, &[]),
    ));
    statements.push(build_renamed_function_declaration_statement(
        builder,
        allocator,
        function,
        &optimized_name,
        Some((compiled_params, function_body)),
        true,
    ));
    statements.push(build_renamed_function_declaration_statement(
        builder,
        allocator,
        function,
        &unoptimized_name,
        None,
        false,
    ));
    statements.push(build_gate_wrapper_function_statement(
        builder,
        function.span,
        &cf.name,
        &gate_result_name,
        &optimized_name,
        &unoptimized_name,
        &wrapper_args,
    ));
    Some(statements)
}

fn function_declaration_to_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
) -> ast::Expression<'a> {
    let mut cloned = function.clone_in(allocator);
    cloned.r#type = ast::FunctionType::FunctionExpression;
    strip_compiled_function_signature_types(&mut cloned);
    ast::Expression::FunctionExpression(builder.alloc(cloned))
}

fn build_const_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    build_const_binding_statement(builder, span, name, init)
}

fn build_const_binding_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(name));
    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        span,
        ast::VariableDeclarationKind::Const,
        builder.vec1(builder.variable_declarator(
            span,
            ast::VariableDeclarationKind::Const,
            pattern,
            NONE,
            Some(init),
            false,
        )),
        false,
    ))
}

fn build_exported_const_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(name));
    let declaration = ast::Declaration::VariableDeclaration(builder.alloc_variable_declaration(
        span,
        ast::VariableDeclarationKind::Const,
        builder.vec1(builder.variable_declarator(
            span,
            ast::VariableDeclarationKind::Const,
            pattern,
            NONE,
            Some(init),
            false,
        )),
        false,
    ));
    ast::Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
        span,
        Some(declaration),
        builder.vec(),
        None,
        ast::ImportOrExportKind::Value,
        NONE,
    ))
}

fn build_export_default_identifier_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
) -> ast::Statement<'a> {
    ast::Statement::ExportDefaultDeclaration(builder.alloc_export_default_declaration(
        span,
        ast::ExportDefaultDeclarationKind::Identifier(
            builder.alloc_identifier_reference(span, builder.atom(name)),
        ),
    ))
}

fn build_renamed_function_declaration_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    name: &str,
    optimized: Option<(&[CompiledParam], &ast::FunctionBody<'a>)>,
    strip_signature_types: bool,
) -> ast::Statement<'a> {
    let mut cloned = function.clone_in(allocator);
    cloned.id = Some(builder.binding_identifier(function.span, builder.atom(name)));
    if strip_signature_types {
        strip_compiled_function_signature_types(&mut cloned);
    }
    if let Some((compiled_params, function_body)) = optimized {
        cloned.params = make_compiled_formal_params(builder, cloned.params.kind, compiled_params);
        cloned.body = Some(make_function_body(builder, allocator, function_body));
    }
    ast::Statement::FunctionDeclaration(builder.alloc(cloned))
}

fn build_gate_wrapper_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    gate_result_name: &str,
    optimized_name: &str,
    unoptimized_name: &str,
    wrapper_args: &[String],
) -> ast::Statement<'a> {
    let mut function = ast::Function::dummy(builder.allocator);
    function.span = span;
    function.r#type = ast::FunctionType::FunctionDeclaration;
    function.id = Some(builder.binding_identifier(span, builder.atom(name)));
    function.params = build_wrapper_formal_params(builder, span, wrapper_args);
    let test = builder.expression_identifier(span, builder.ident(gate_result_name));
    let optimized_call =
        build_identifier_call_expression(builder, span, optimized_name, wrapper_args);
    let unoptimized_call =
        build_identifier_call_expression(builder, span, unoptimized_name, wrapper_args);
    function.body = Some(builder.alloc_function_body(
        span,
        builder.vec(),
        builder.vec1(builder.statement_if(
            span,
            test,
            builder.statement_return(span, Some(optimized_call)),
            Some(builder.statement_return(span, Some(unoptimized_call))),
        )),
    ));
    ast::Statement::FunctionDeclaration(builder.alloc(function))
}

fn build_wrapper_formal_params<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    wrapper_args: &[String],
) -> oxc_allocator::Box<'a, ast::FormalParameters<'a>> {
    let items = builder.vec_from_iter(wrapper_args.iter().map(|arg| {
        let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(arg));
        builder.plain_formal_parameter(span, pattern)
    }));
    builder.alloc(builder.formal_parameters(
        span,
        ast::FormalParameterKind::FormalParameter,
        items,
        NONE,
    ))
}

fn build_identifier_call_expression<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    args: &[String],
) -> ast::Expression<'a> {
    let arguments =
        builder.vec_from_iter(args.iter().map(|arg| {
            ast::Argument::from(builder.expression_identifier(span, builder.ident(arg)))
        }));
    builder.expression_call(
        span,
        builder.expression_identifier(span, builder.ident(name)),
        NONE,
        arguments,
        false,
    )
}

fn replace_compiled_function_in_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    statement: &mut ast::Statement<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match statement {
        ast::Statement::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            strip_compiled_function_signature_types(function);
            function.params =
                make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(make_function_body(builder, allocator, function_body));
            true
        }
        ast::Statement::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator(
                    builder,
                    allocator,
                    declarator,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ExpressionStatement(expression_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut expression_statement.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::ReturnStatement(return_statement) => {
            return_statement.argument.as_mut().is_some_and(|argument| {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ThrowStatement(throw_statement) => replace_compiled_function_in_expression(
            builder,
            allocator,
            &mut throw_statement.argument,
            cf,
            compiled_params,
            function_body,
        ),
        ast::Statement::BlockStatement(block_statement) => {
            block_statement.body.iter_mut().any(|statement| {
                replace_compiled_function_in_statement(
                    builder,
                    allocator,
                    statement,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::IfStatement(if_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut if_statement.test,
                cf,
                compiled_params,
                function_body,
            ) || replace_compiled_function_in_statement(
                builder,
                allocator,
                &mut if_statement.consequent,
                cf,
                compiled_params,
                function_body,
            ) || if_statement.alternate.as_mut().is_some_and(|alternate| {
                replace_compiled_function_in_statement(
                    builder,
                    allocator,
                    alternate,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::LabeledStatement(labeled_statement) => {
            replace_compiled_function_in_statement(
                builder,
                allocator,
                &mut labeled_statement.body,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::SwitchStatement(switch_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut switch_statement.discriminant,
                cf,
                compiled_params,
                function_body,
            ) || switch_statement.cases.iter_mut().any(|case| {
                case.test.as_mut().is_some_and(|test| {
                    replace_compiled_function_in_expression(
                        builder,
                        allocator,
                        test,
                        cf,
                        compiled_params,
                        function_body,
                    )
                }) || case.consequent.iter_mut().any(|statement| {
                    replace_compiled_function_in_statement(
                        builder,
                        allocator,
                        statement,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
            })
        }
        ast::Statement::ExportNamedDeclaration(export_named) => export_named
            .declaration
            .as_mut()
            .is_some_and(|declaration| {
                replace_compiled_function_in_declaration(
                    builder,
                    allocator,
                    declaration,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Statement::ExportDefaultDeclaration(export_default) => {
            replace_compiled_function_in_export_default(
                builder,
                allocator,
                export_default,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_statement_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    statement: &mut ast::Statement<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match statement {
        ast::Statement::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator_with_gate(
                    builder,
                    allocator,
                    declarator,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ExpressionStatement(expression_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut expression_statement.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::ReturnStatement(return_statement) => {
            return_statement.argument.as_mut().is_some_and(|argument| {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ThrowStatement(throw_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut throw_statement.argument,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::BlockStatement(block_statement) => {
            block_statement.body.iter_mut().any(|statement| {
                replace_compiled_function_in_statement_with_gate(
                    builder,
                    allocator,
                    statement,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::IfStatement(if_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut if_statement.test,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || replace_compiled_function_in_statement_with_gate(
                builder,
                allocator,
                &mut if_statement.consequent,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || if_statement.alternate.as_mut().is_some_and(|alternate| {
                replace_compiled_function_in_statement_with_gate(
                    builder,
                    allocator,
                    alternate,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::LabeledStatement(labeled_statement) => {
            replace_compiled_function_in_statement_with_gate(
                builder,
                allocator,
                &mut labeled_statement.body,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::SwitchStatement(switch_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut switch_statement.discriminant,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || switch_statement.cases.iter_mut().any(|case| {
                case.test.as_mut().is_some_and(|test| {
                    replace_compiled_function_in_expression_with_gate(
                        builder,
                        allocator,
                        test,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                }) || case.consequent.iter_mut().any(|statement| {
                    replace_compiled_function_in_statement_with_gate(
                        builder,
                        allocator,
                        statement,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
            })
        }
        ast::Statement::ExportNamedDeclaration(export_named) => export_named
            .declaration
            .as_mut()
            .is_some_and(|declaration| {
                replace_compiled_function_in_declaration_with_gate(
                    builder,
                    allocator,
                    declaration,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Statement::ExportDefaultDeclaration(export_default) => {
            replace_compiled_function_in_export_default_with_gate(
                builder,
                allocator,
                export_default,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declaration<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declaration: &mut ast::Declaration<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match declaration {
        ast::Declaration::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            strip_compiled_function_signature_types(function);
            function.params =
                make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(make_function_body(builder, allocator, function_body));
            true
        }
        ast::Declaration::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator(
                    builder,
                    allocator,
                    declarator,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declaration_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declaration: &mut ast::Declaration<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match declaration {
        ast::Declaration::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator_with_gate(
                    builder,
                    allocator,
                    declarator,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        _ => false,
    }
}

fn replace_compiled_function_in_export_default<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    export_default: &mut ast::ExportDefaultDeclaration<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match &mut export_default.declaration {
        ast::ExportDefaultDeclarationKind::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            strip_compiled_function_signature_types(function);
            function.params =
                make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(make_function_body(builder, allocator, function_body));
            true
        }
        ast::ExportDefaultDeclarationKind::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            strip_compiled_function_signature_types(function);
            function.params =
                make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(make_function_body(builder, allocator, function_body));
            true
        }
        ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            strip_compiled_arrow_signature_types(arrow);
            arrow.params = make_compiled_formal_params(builder, arrow.params.kind, compiled_params);
            arrow.expression = false;
            arrow.body = make_function_body(builder, allocator, function_body);
            true
        }
        ast::ExportDefaultDeclarationKind::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression(
                builder,
                allocator,
                call_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression(
                builder,
                allocator,
                assignment_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::SequenceExpression(sequence_expression) => {
            sequence_expression
                .expressions
                .iter_mut()
                .any(|expression| {
                    replace_compiled_function_in_expression(
                        builder,
                        allocator,
                        expression,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
        }
        ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression(
                builder,
                allocator,
                conditional_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression(
                builder,
                allocator,
                logical_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression(
                builder,
                allocator,
                array_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression(
                builder,
                allocator,
                object_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut type_assertion.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_export_default_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    export_default: &mut ast::ExportDefaultDeclaration<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match &mut export_default.declaration {
        ast::ExportDefaultDeclarationKind::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            let original = ast::Expression::FunctionExpression(function.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            export_default.declaration = convert_expression_to_export_default_kind(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            let original = ast::Expression::ArrowFunctionExpression(arrow.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            export_default.declaration = convert_expression_to_export_default_kind(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::ExportDefaultDeclarationKind::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression_with_gate(
                builder,
                allocator,
                call_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression_with_gate(
                builder,
                allocator,
                assignment_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::SequenceExpression(sequence_expression) => {
            sequence_expression
                .expressions
                .iter_mut()
                .any(|expression| {
                    replace_compiled_function_in_expression_with_gate(
                        builder,
                        allocator,
                        expression,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
        }
        ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression_with_gate(
                builder,
                allocator,
                conditional_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression_with_gate(
                builder,
                allocator,
                logical_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression_with_gate(
                builder,
                allocator,
                array_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression_with_gate(
                builder,
                allocator,
                object_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut type_assertion.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declarator<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declarator: &mut ast::VariableDeclarator<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    let Some(init) = declarator.init.as_mut() else {
        return false;
    };
    replace_compiled_function_in_expression(
        builder,
        allocator,
        init,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_declarator_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declarator: &mut ast::VariableDeclarator<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    let Some(init) = declarator.init.as_mut() else {
        return false;
    };
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        init,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match expression {
        ast::Expression::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            strip_compiled_function_signature_types(function);
            function.params =
                make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(make_function_body(builder, allocator, function_body));
            true
        }
        ast::Expression::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            strip_compiled_arrow_signature_types(arrow);
            arrow.params = make_compiled_formal_params(builder, arrow.params.kind, compiled_params);
            arrow.expression = false;
            arrow.body = make_function_body(builder, allocator, function_body);
            true
        }
        ast::Expression::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression(
                builder,
                allocator,
                call_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression(
                builder,
                allocator,
                assignment_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::SequenceExpression(sequence_expression) => sequence_expression
            .expressions
            .iter_mut()
            .any(|expression| {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    expression,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Expression::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression(
                builder,
                allocator,
                conditional_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression(
                builder,
                allocator,
                logical_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression(
                builder,
                allocator,
                array_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression(
                builder,
                allocator,
                object_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut type_assertion.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match expression {
        ast::Expression::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            let original = ast::Expression::FunctionExpression(function.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            *expression = make_gate_conditional_expression(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::Expression::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            let original = ast::Expression::ArrowFunctionExpression(arrow.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            *expression = make_gate_conditional_expression(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::Expression::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression_with_gate(
                builder,
                allocator,
                call_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression_with_gate(
                builder,
                allocator,
                assignment_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::SequenceExpression(sequence_expression) => sequence_expression
            .expressions
            .iter_mut()
            .any(|expression| {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    expression,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Expression::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression_with_gate(
                builder,
                allocator,
                conditional_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression_with_gate(
                builder,
                allocator,
                logical_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression_with_gate(
                builder,
                allocator,
                array_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression_with_gate(
                builder,
                allocator,
                object_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut type_assertion.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_call_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    call_expression: &mut ast::CallExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut call_expression.callee,
        cf,
        compiled_params,
        function_body,
    ) || call_expression.arguments.iter_mut().any(|argument| {
        replace_compiled_function_in_argument(
            builder,
            allocator,
            argument,
            cf,
            compiled_params,
            function_body,
        )
    })
}

fn replace_compiled_function_in_call_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    call_expression: &mut ast::CallExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut call_expression.callee,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || call_expression.arguments.iter_mut().any(|argument| {
        replace_compiled_function_in_argument_with_gate(
            builder,
            allocator,
            argument,
            gate_name,
            cf,
            compiled_params,
            function_body,
        )
    })
}

fn replace_compiled_function_in_assignment_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    assignment_expression: &mut ast::AssignmentExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut assignment_expression.right,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_assignment_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    assignment_expression: &mut ast::AssignmentExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut assignment_expression.right,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_conditional_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    conditional_expression: &mut ast::ConditionalExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.test,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.consequent,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.alternate,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_conditional_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    conditional_expression: &mut ast::ConditionalExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.test,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.consequent,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.alternate,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_logical_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    logical_expression: &mut ast::LogicalExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut logical_expression.left,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut logical_expression.right,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_logical_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    logical_expression: &mut ast::LogicalExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut logical_expression.left,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut logical_expression.right,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_array_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    array_expression: &mut ast::ArrayExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    array_expression
        .elements
        .iter_mut()
        .any(|element| match element {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut spread.argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => {
                let element_expression: &mut ast::Expression<'a> =
                    unsafe { std::mem::transmute(element) };
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    element_expression,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_array_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    array_expression: &mut ast::ArrayExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    array_expression
        .elements
        .iter_mut()
        .any(|element| match element {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut spread.argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => {
                let element_expression: &mut ast::Expression<'a> =
                    unsafe { std::mem::transmute(element) };
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    element_expression,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_object_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    object_expression: &mut ast::ObjectExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    object_expression
        .properties
        .iter_mut()
        .any(|property| match property {
            ast::ObjectPropertyKind::ObjectProperty(property) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut property.value,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ObjectPropertyKind::SpreadProperty(spread) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut spread.argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_object_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    object_expression: &mut ast::ObjectExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    object_expression
        .properties
        .iter_mut()
        .any(|property| match property {
            ast::ObjectPropertyKind::ObjectProperty(property) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut property.value,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ObjectPropertyKind::SpreadProperty(spread) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut spread.argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_argument<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    argument: &mut ast::Argument<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match argument {
        ast::Argument::SpreadElement(spread) => replace_compiled_function_in_expression(
            builder,
            allocator,
            &mut spread.argument,
            cf,
            compiled_params,
            function_body,
        ),
        _ => {
            let argument_expression: &mut ast::Expression<'a> =
                unsafe { std::mem::transmute(argument) };
            replace_compiled_function_in_expression(
                builder,
                allocator,
                argument_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
    }
}

fn replace_compiled_function_in_argument_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    argument: &mut ast::Argument<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match argument {
        ast::Argument::SpreadElement(spread) => replace_compiled_function_in_expression_with_gate(
            builder,
            allocator,
            &mut spread.argument,
            gate_name,
            cf,
            compiled_params,
            function_body,
        ),
        _ => {
            let argument_expression: &mut ast::Expression<'a> =
                unsafe { std::mem::transmute(argument) };
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                argument_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
    }
}

fn make_gate_conditional_expression<'a>(
    builder: AstBuilder<'a>,
    gate_name: &str,
    span: oxc_span::Span,
    consequent: ast::Expression<'a>,
    alternate: ast::Expression<'a>,
) -> ast::Expression<'a> {
    let gate_call = builder.expression_call(
        span,
        builder.expression_identifier(span, builder.ident(gate_name)),
        NONE,
        builder.vec(),
        false,
    );
    builder.expression_conditional(span, gate_call, consequent, alternate)
}

fn convert_expression_to_export_default_kind<'a>(
    builder: AstBuilder<'a>,
    gate_name: &str,
    span: oxc_span::Span,
    consequent: ast::Expression<'a>,
    alternate: ast::Expression<'a>,
) -> ast::ExportDefaultDeclarationKind<'a> {
    let conditional =
        make_gate_conditional_expression(builder, gate_name, span, consequent, alternate);
    match conditional {
        ast::Expression::FunctionExpression(function) => {
            ast::ExportDefaultDeclarationKind::FunctionExpression(function)
        }
        ast::Expression::ArrowFunctionExpression(arrow) => {
            ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
        }
        ast::Expression::CallExpression(call) => {
            ast::ExportDefaultDeclarationKind::CallExpression(call)
        }
        ast::Expression::ConditionalExpression(conditional) => {
            ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional)
        }
        ast::Expression::AssignmentExpression(assignment) => {
            ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment)
        }
        ast::Expression::ParenthesizedExpression(parenthesized) => {
            ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized)
        }
        ast::Expression::SequenceExpression(sequence) => {
            ast::ExportDefaultDeclarationKind::SequenceExpression(sequence)
        }
        ast::Expression::LogicalExpression(logical) => {
            ast::ExportDefaultDeclarationKind::LogicalExpression(logical)
        }
        ast::Expression::ArrayExpression(array) => {
            ast::ExportDefaultDeclarationKind::ArrayExpression(array)
        }
        ast::Expression::ObjectExpression(object) => {
            ast::ExportDefaultDeclarationKind::ObjectExpression(object)
        }
        ast::Expression::TSAsExpression(ts_as) => {
            ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as)
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies) => {
            ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies)
        }
        ast::Expression::TSNonNullExpression(ts_non_null) => {
            ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null)
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion)
        }
        ast::Expression::TSInstantiationExpression(instantiation) => {
            ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation)
        }
        other => panic!("unsupported export default gated expression: {:?}", other),
    }
}

fn strip_compiled_function_signature_types(function: &mut ast::Function<'_>) {
    function.type_parameters = None;
    function.this_param = None;
    function.return_type = None;
}

fn strip_compiled_arrow_signature_types(arrow: &mut ast::ArrowFunctionExpression<'_>) {
    arrow.type_parameters = None;
    arrow.return_type = None;
}

fn make_compiled_formal_params<'a>(
    builder: AstBuilder<'a>,
    kind: ast::FormalParameterKind,
    compiled_params: &[CompiledParam],
) -> oxc_allocator::Box<'a, ast::FormalParameters<'a>> {
    let mut items = builder.vec();
    let mut rest = None;
    for param in compiled_params {
        let pattern = builder.binding_pattern_binding_identifier(SPAN, builder.ident(&param.name));
        if param.is_rest {
            rest = Some(builder.alloc_formal_parameter_rest(
                SPAN,
                builder.vec(),
                builder.binding_rest_element(SPAN, pattern),
                NONE,
            ));
        } else {
            items.push(builder.plain_formal_parameter(SPAN, pattern));
        }
    }
    builder.alloc(builder.formal_parameters(SPAN, kind, items, rest))
}

fn make_function_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function_body: &ast::FunctionBody<'a>,
) -> oxc_allocator::Box<'a, ast::FunctionBody<'a>> {
    builder.alloc(function_body.clone_in(allocator))
}

fn build_compiled_function_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
    state: &AstRenderState,
    original_body: Option<&ast::FunctionBody<'_>>,
) -> Option<ast::FunctionBody<'a>> {
    let mut function_body = if let Some(function_body) =
        try_build_compiled_function_body_from_hir(builder, cf, state)
    {
        function_body
    } else if let Some(function_body) =
        try_build_compiled_function_body_from_shape(builder, allocator, source_type, cf)
    {
        function_body
    } else if let Some(body_source) = cf.generated_body.as_ref() {
        parse_compiled_function_body(allocator, source_type, cf, body_source).ok()?
    } else if let Some(default_cache) = cf.synthesized_default_param_cache.as_ref() {
        build_default_param_cache_seed_body(builder, default_cache)
    } else {
        return None;
    };

    normalize_use_fire_binding_temps_ast(builder, &mut function_body, cf);
    wrap_function_hook_guard_body(builder, allocator, &mut function_body, cf, state);
    apply_preserved_directives(builder, &mut function_body, &cf.directives);
    prepend_cache_prologue_statements(
        builder,
        allocator,
        &mut function_body,
        cf.cache_prologue.as_ref(),
        state,
    );
    prepend_synthesized_default_param_cache_statements(
        builder,
        allocator,
        source_type,
        &mut function_body,
        cf,
    )?;
    prepend_compiled_body_prefix_statements(
        builder,
        allocator,
        source_type,
        &mut function_body,
        cf,
        original_body,
        Some(&state.cache_import_name),
    )?;
    prepend_instrument_forget_statement(builder, allocator, &mut function_body, cf, state);
    align_runtime_identifier_references(builder, &mut function_body, cf, state);
    apply_emit_freeze_to_cache_stores_ast(builder, allocator, &mut function_body, cf, state);
    Some(function_body)
}

fn try_build_compiled_function_body_from_hir<'a>(
    builder: AstBuilder<'a>,
    cf: &CompiledFunction,
    _state: &AstRenderState,
) -> Option<ast::FunctionBody<'a>> {
    if cf.body_payload != CompiledBodyPayload::LowerFromFinalHir || cf.needs_cache_import {
        return None;
    }
    let hir_function = cf.hir_function.as_ref()?;
    let statements = super::hir_to_ast::try_lower_function_body_ast(builder, hir_function)?;
    Some(builder.function_body(SPAN, builder.vec(), statements))
}

fn build_default_param_cache_seed_body<'a>(
    builder: AstBuilder<'a>,
    default_cache: &SynthesizedDefaultParamCache,
) -> ast::FunctionBody<'a> {
    builder.function_body(
        SPAN,
        builder.vec(),
        builder.vec1(builder.statement_return(
            SPAN,
            Some(builder.expression_identifier(SPAN, builder.ident(&default_cache.value_name))),
        )),
    )
}

fn try_build_compiled_function_body_from_shape<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
) -> Option<ast::FunctionBody<'a>> {
    try_build_function_body_from_shape(
        builder,
        allocator,
        source_type,
        &cf.generated_body_shape,
        cf.cache_prologue.as_ref(),
    )
}

fn build_generated_binding_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    bindings: &[crate::reactive_scopes::codegen_reactive::GeneratedBinding],
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let mut statements = builder.vec();
    for binding in bindings {
        let expression =
            parse_expression_source(allocator, source_type, &binding.expression).ok()?;
        let pattern =
            parse_binding_pattern_source(allocator, source_type, &binding.pattern).ok()?;
        statements.push(ast::Statement::VariableDeclaration(
            builder.alloc_variable_declaration(
                SPAN,
                binding.kind,
                builder.vec1(builder.variable_declarator(
                    SPAN,
                    binding.kind,
                    pattern,
                    NONE,
                    Some(expression),
                    false,
                )),
                false,
            ),
        ));
    }
    Some(statements)
}

fn build_generated_declaration_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    declarations: &[crate::reactive_scopes::codegen_reactive::GeneratedDeclaration],
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let mut statements = builder.vec();
    for declaration in declarations {
        let pattern =
            parse_binding_pattern_source(allocator, source_type, &declaration.pattern).ok()?;
        statements.push(ast::Statement::VariableDeclaration(
            builder.alloc_variable_declaration(
                SPAN,
                declaration.kind,
                builder.vec1(builder.variable_declarator(
                    SPAN,
                    declaration.kind,
                    pattern,
                    NONE,
                    None,
                    false,
                )),
                false,
            ),
        ));
    }
    Some(statements)
}

fn build_generated_assignment_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    assignments: &[crate::reactive_scopes::codegen_reactive::GeneratedAssignment],
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let mut statements = builder.vec();
    for assignment in assignments {
        let target =
            parse_assignment_target_source(allocator, source_type, &assignment.target).ok()?;
        let value = parse_expression_source(allocator, source_type, &assignment.value).ok()?;
        statements.push(builder.statement_expression(
            SPAN,
            builder.expression_assignment(SPAN, AssignmentOperator::Assign, target, value),
        ));
    }
    Some(statements)
}

fn build_generated_expression_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    expressions: &[String],
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let mut statements = builder.vec();
    for expression in expressions {
        let expression = parse_expression_source(allocator, source_type, expression).ok()?;
        statements.push(builder.statement_expression(SPAN, expression));
    }
    Some(statements)
}

fn build_generated_statement_sources<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    statements: &[String],
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if statements.is_empty() {
        return Some(builder.vec());
    }
    let joined = statements.join("\n");
    let ts_source_type = source_type.with_typescript(true);
    let mut attempts = vec![
        (source_type, joined.clone()),
        (ts_source_type, joined.clone()),
    ];
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(&joined);
    if flow_cast_normalized != joined {
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
    }
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(&joined);
    if flow_cast_rewritten != joined && flow_cast_rewritten != flow_cast_normalized {
        attempts.push((ts_source_type, flow_cast_rewritten));
    }
    for (attempt_source_type, attempt_body) in attempts {
        if let Ok(parsed) = parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&attempt_body),
        ) {
            return Some(parsed);
        }
    }
    None
}

fn build_generated_switch_cases<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cases: &[crate::reactive_scopes::codegen_reactive::GeneratedSwitchCase],
    cache_prologue: Option<&crate::reactive_scopes::codegen_reactive::CachePrologue>,
) -> Option<oxc_allocator::Vec<'a, ast::SwitchCase<'a>>> {
    let mut rendered_cases = builder.vec();
    for case in cases {
        let consequent = try_build_function_body_from_shape(
            builder,
            allocator,
            source_type,
            &case.consequent,
            cache_prologue,
        )?
        .statements;
        let test = case
            .test
            .as_deref()
            .map(|test| parse_expression_source(allocator, source_type, test))
            .transpose()
            .ok()?;
        rendered_cases.push(builder.switch_case(SPAN, test, consequent));
    }
    Some(rendered_cases)
}

fn replace_final_return_expression<'a>(
    body: &mut ast::FunctionBody<'a>,
    expression: ast::Expression<'a>,
) -> Option<()> {
    let ast::Statement::ReturnStatement(return_statement) = body.statements.last_mut()? else {
        return None;
    };
    return_statement.argument = Some(expression);
    Some(())
}

fn try_build_function_body_from_shape<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body_shape: &crate::reactive_scopes::codegen_reactive::GeneratedBodyShape,
    cache_prologue: Option<&crate::reactive_scopes::codegen_reactive::CachePrologue>,
) -> Option<ast::FunctionBody<'a>> {
    match body_shape {
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Unknown => None,
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Block { inner } => {
            let inner = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_block(SPAN, inner.statements)),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Labeled { label, inner } => {
            let inner = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(ast::Statement::LabeledStatement(
                    builder.alloc_labeled_statement(
                        SPAN,
                        builder.label_identifier(SPAN, builder.atom(label)),
                        builder.statement_block(SPAN, inner.statements),
                    ),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Switch {
            discriminant,
            cases,
        } => Some(builder.function_body(
            SPAN,
            builder.vec(),
            builder.vec1(builder.statement_switch(
                SPAN,
                parse_expression_source(allocator, source_type, discriminant).ok()?,
                build_generated_switch_cases(
                    builder,
                    allocator,
                    source_type,
                    cases,
                    cache_prologue,
                )?,
            )),
        )),
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ExpressionStatements(
            expressions,
        ) => Some(builder.function_body(
            SPAN,
            builder.vec(),
            build_generated_expression_statements(builder, allocator, source_type, expressions)?,
        )),
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AssignmentStatements(
            assignments,
        ) => Some(builder.function_body(
            SPAN,
            builder.vec(),
            build_generated_assignment_statements(builder, allocator, source_type, assignments)?,
        )),
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::GuardedBody {
            test,
            inner,
        } => {
            let inner = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_if(
                    SPAN,
                    parse_expression_source(allocator, source_type, test).ok()?,
                    builder.statement_block(SPAN, inner.statements),
                    None,
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::GuardedExpressionStatements {
            test,
            expressions,
        } => Some(builder.function_body(
            SPAN,
            builder.vec(),
            builder.vec1(builder.statement_if(
                SPAN,
                parse_expression_source(allocator, source_type, test).ok()?,
                builder.statement_block(
                    SPAN,
                    build_generated_expression_statements(
                        builder,
                        allocator,
                        source_type,
                        expressions,
                    )?,
                ),
                None,
            )),
        )),
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::WhileLoop {
            test,
            body,
        } => {
            let body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                body.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_while(
                    SPAN,
                    parse_expression_source(allocator, source_type, test).ok()?,
                    builder.statement_block(SPAN, body.statements),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::DoWhileLoop {
            test,
            body,
        } => {
            let body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                body.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_do_while(
                    SPAN,
                    builder.statement_block(SPAN, body.statements),
                    parse_expression_source(allocator, source_type, test).ok()?,
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ForLoop {
            init,
            test,
            update,
            body,
        } => {
            let body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                body.as_ref(),
                cache_prologue,
            )?;
            let init = match init.as_deref() {
                Some(init) => crate::reactive_scopes::codegen_reactive::parse_for_statement_init_ast(
                    allocator,
                    init,
                )?,
                None => None,
            };
            let test = test
                .as_deref()
                .and_then(|test| parse_expression_source(allocator, source_type, test).ok());
            let update = update
                .as_deref()
                .and_then(|update| parse_expression_source(allocator, source_type, update).ok());
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_for(
                    SPAN,
                    init,
                    test,
                    update,
                    builder.statement_block(SPAN, body.statements),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ForInLoop {
            left,
            right,
            body,
        } => {
            let body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                body.as_ref(),
                cache_prologue,
            )?;
            let left =
                crate::reactive_scopes::codegen_reactive::parse_for_statement_left_source_ast(
                    allocator, left, false,
                )?;
            let right = parse_expression_source(allocator, source_type, right).ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_for_in(
                    SPAN,
                    left,
                    right,
                    builder.statement_block(SPAN, body.statements),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ForOfLoop {
            left,
            right,
            body,
        } => {
            let body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                body.as_ref(),
                cache_prologue,
            )?;
            let left =
                crate::reactive_scopes::codegen_reactive::parse_for_statement_left_source_ast(
                    allocator, left, true,
                )?;
            let right = parse_expression_source(allocator, source_type, right).ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_for_of(
                    SPAN,
                    false,
                    left,
                    right,
                    builder.statement_block(SPAN, body.statements),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::GuardedReturnPrefix {
            test,
            consequent,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let consequent = consequent
                .as_deref()
                .map(|expr| parse_expression_source(allocator, source_type, expr))
                .transpose()
                .ok()?;
            body.statements.insert(
                0,
                builder.statement_if(
                    SPAN,
                    parse_expression_source(allocator, source_type, test).ok()?,
                    builder.statement_block(
                        SPAN,
                        builder.vec1(builder.statement_return(SPAN, consequent)),
                    ),
                    None,
                ),
            );
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ConditionalBranches {
            test,
            consequent,
            alternate,
        } => {
            let consequent = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                consequent.as_ref(),
                cache_prologue,
            )?;
            let alternate = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                alternate.as_ref(),
                cache_prologue,
            )?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_if(
                    SPAN,
                    parse_expression_source(allocator, source_type, test).ok()?,
                    builder.statement_block(SPAN, consequent.statements),
                    Some(builder.statement_block(SPAN, alternate.statements)),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::GuardedAssignments {
            test,
            assignments,
        } => Some(builder.function_body(
            SPAN,
            builder.vec(),
            builder.vec1(builder.statement_if(
                SPAN,
                parse_expression_source(allocator, source_type, test).ok()?,
                builder.statement_block(
                    SPAN,
                    build_generated_assignment_statements(
                        builder,
                        allocator,
                        source_type,
                        assignments,
                    )?,
                ),
                None,
            )),
        )),
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::GuardedAssignmentExpressions {
            test,
            assignments,
            expressions,
        } => {
            let mut guarded = build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                assignments,
            )?;
            guarded.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                expressions,
            )?);
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_if(
                    SPAN,
                    parse_expression_source(allocator, source_type, test).ok()?,
                    builder.statement_block(SPAN, guarded),
                    None,
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedCachedValues {
            sentinel_slot,
            setup_statements,
            cached_values,
            restored_values,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut consequent = build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                setup_statements,
            )?;
            for value in cached_values {
                consequent.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    value.slot,
                    builder.expression_identifier(SPAN, builder.ident(&value.name)),
                ));
            }

            let mut alternate = builder.vec();
            for value in restored_values {
                alternate.push(build_identifier_assignment_statement(
                    builder,
                    &value.name,
                    cache_member_slot_expression(builder, cache_binding_name, value.slot),
                ));
            }

            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_if(
                    SPAN,
                    builder.expression_binary(
                        SPAN,
                        cache_member_slot_expression(builder, cache_binding_name, *sentinel_slot),
                        BinaryOperator::StrictEquality,
                        build_memo_cache_sentinel_expression(builder),
                    ),
                    builder.statement_block(SPAN, consequent),
                    Some(builder.statement_block(SPAN, alternate)),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::MemoizedCachedValues {
            deps,
            setup_statements,
            cached_values,
            restored_values,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut dep_assignments = builder.vec();
            let mut dep_guards = deps.iter().map(|(slot, dep_expr)| {
                let dep_expression = parse_expression_source(allocator, source_type, dep_expr).ok()?;
                dep_assignments.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    *slot,
                    dep_expression.clone_in(allocator),
                ));
                Some(builder.expression_binary(
                    SPAN,
                    cache_member_slot_expression(builder, cache_binding_name, *slot),
                    BinaryOperator::StrictInequality,
                    dep_expression,
                ))
            });
            let mut test = dep_guards.next()??;
            for guard in dep_guards {
                test = builder.expression_logical(SPAN, test, LogicalOperator::Or, guard?);
            }

            let mut consequent = build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                setup_statements,
            )?;
            consequent.extend(dep_assignments);
            for value in cached_values {
                consequent.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    value.slot,
                    builder.expression_identifier(SPAN, builder.ident(&value.name)),
                ));
            }

            let mut alternate = builder.vec();
            for value in restored_values {
                alternate.push(build_identifier_assignment_statement(
                    builder,
                    &value.name,
                    cache_member_slot_expression(builder, cache_binding_name, value.slot),
                ));
            }

            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_if(
                    SPAN,
                    test,
                    builder.statement_block(SPAN, consequent),
                    Some(builder.statement_block(SPAN, alternate)),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::MemoizedEarlyReturnSentinel {
            deps,
            setup_statements,
            cached_values,
            restored_values,
            sentinel_name,
            final_return,
            fallback_body,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut dep_assignments = builder.vec();
            let mut dep_guards = deps.iter().map(|(slot, dep_expr)| {
                let dep_expression = parse_expression_source(allocator, source_type, dep_expr).ok()?;
                dep_assignments.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    *slot,
                    dep_expression.clone_in(allocator),
                ));
                Some(builder.expression_binary(
                    SPAN,
                    cache_member_slot_expression(builder, cache_binding_name, *slot),
                    BinaryOperator::StrictInequality,
                    dep_expression,
                ))
            });
            let mut test = dep_guards.next()??;
            for guard in dep_guards {
                test = builder.expression_logical(SPAN, test, LogicalOperator::Or, guard?);
            }

            let mut consequent = build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                setup_statements,
            )?;
            consequent.extend(dep_assignments);
            for value in cached_values {
                consequent.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    value.slot,
                    builder.expression_identifier(SPAN, builder.ident(&value.name)),
                ));
            }

            let mut alternate = builder.vec();
            for value in restored_values {
                alternate.push(build_identifier_assignment_statement(
                    builder,
                    &value.name,
                    cache_member_slot_expression(builder, cache_binding_name, value.slot),
                ));
            }

            let mut body = builder.vec_from_iter([
                builder.statement_if(
                    SPAN,
                    test,
                    builder.statement_block(SPAN, consequent),
                    Some(builder.statement_block(SPAN, alternate)),
                ),
                builder.statement_if(
                    SPAN,
                    builder.expression_binary(
                        SPAN,
                        builder.expression_identifier(SPAN, builder.ident(sentinel_name)),
                        BinaryOperator::StrictInequality,
                        build_early_return_sentinel_expression(builder),
                    ),
                    builder.statement_block(
                        SPAN,
                        builder.vec1(builder.statement_return(
                            SPAN,
                            Some(builder.expression_identifier(SPAN, builder.ident(sentinel_name))),
                        )),
                    ),
                    None,
                ),
            ]);

            if let Some(final_return) = final_return {
                body.push(builder.statement_return(
                    SPAN,
                    Some(builder.expression_identifier(SPAN, builder.ident(final_return))),
                ));
            } else if let Some(fallback_body) = fallback_body {
                let fallback_body = try_build_function_body_from_shape(
                    builder,
                    allocator,
                    source_type,
                    fallback_body.as_ref(),
                    cache_prologue,
                )?;
                body.extend(fallback_body.statements);
            }

            Some(builder.function_body(SPAN, builder.vec(), body))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::TryCatch {
            catch_param,
            try_body,
            catch_body,
        } => {
            let try_body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                try_body.as_ref(),
                cache_prologue,
            )?;
            let catch_body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                catch_body.as_ref(),
                cache_prologue,
            )?;
            let catch_param = catch_param.as_ref().map(|name| {
                builder.catch_parameter(
                    SPAN,
                    builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)),
                    NONE,
                )
            });
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_try(
                    SPAN,
                    builder.block_statement(SPAN, try_body.statements),
                    Some(builder.alloc_catch_clause(
                        SPAN,
                        catch_param,
                        builder.block_statement(SPAN, catch_body.statements),
                    )),
                    Option::<oxc_allocator::Box<'_, ast::BlockStatement<'_>>>::None,
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Break(label) => {
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_break(
                    SPAN,
                    label
                        .as_ref()
                        .map(|label| builder.label_identifier(SPAN, builder.atom(label))),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Continue(label) => {
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_continue(
                    SPAN,
                    label
                        .as_ref()
                        .map(|label| builder.label_identifier(SPAN, builder.atom(label))),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnVoid => {
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_return(SPAN, None)),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnIdentifier(name) => {
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_return(
                    SPAN,
                    Some(builder.expression_identifier(SPAN, builder.ident(name))),
                )),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnExpression(
            expression_source,
        ) => {
            let expression = parse_expression_source(allocator, source_type, expression_source)
                .ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_return(SPAN, Some(expression))),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ThrowExpression(
            expression_source,
        ) => {
            let expression = parse_expression_source(allocator, source_type, expression_source)
                .ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(builder.statement_throw(SPAN, expression)),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::BoundExpressionReturn {
            value_name,
            value_kind,
            expression,
        } => {
            let expression = parse_expression_source(allocator, source_type, expression).ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder
                                .binding_pattern_binding_identifier(SPAN, builder.ident(value_name)),
                            NONE,
                            Some(expression),
                            false,
                        )),
                        false,
                    )),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AssignedExpressionReturn {
            value_name,
            value_kind,
            expression,
        } => {
            let expression = parse_expression_source(allocator, source_type, expression).ok()?;
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(value_name),
                            ),
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    )),
                    build_identifier_assignment_statement(
                        builder,
                        value_name,
                        expression,
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
            value_name,
            value_kind,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(value_name),
                            ),
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    )),
                    builder.statement_if(
                        SPAN,
                        builder.expression_binary(
                            SPAN,
                            cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            BinaryOperator::StrictEquality,
                            build_memo_cache_sentinel_expression(builder),
                        ),
                        builder.statement_block(
                            SPAN,
                            consequent,
                        ),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedExistingReturn {
            value_name,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    builder.statement_if(
                        SPAN,
                        builder.expression_binary(
                            SPAN,
                            cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            BinaryOperator::StrictEquality,
                            build_memo_cache_sentinel_expression(builder),
                        ),
                        builder.statement_block(
                            SPAN,
                            consequent,
                        ),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
            value_name,
            value_kind,
            dep_slot,
            dep_expr,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let dep_expr = parse_expression_source(allocator, source_type, dep_expr).ok()?;
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *dep_slot,
                dep_expr.clone_in(allocator),
            ));
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(value_name),
                            ),
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    )),
                    builder.statement_if(
                        SPAN,
                        builder.expression_binary(
                            SPAN,
                            cache_member_slot_expression(builder, cache_binding_name, *dep_slot),
                            BinaryOperator::StrictInequality,
                            dep_expr.clone_in(allocator),
                        ),
                        builder.statement_block(
                            SPAN,
                            consequent,
                        ),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedExistingReturn {
            value_name,
            dep_slot,
            dep_expr,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let dep_expr = parse_expression_source(allocator, source_type, dep_expr).ok()?;
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *dep_slot,
                dep_expr.clone_in(allocator),
            ));
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    builder.statement_if(
                        SPAN,
                        builder.expression_binary(
                            SPAN,
                            cache_member_slot_expression(builder, cache_binding_name, *dep_slot),
                            BinaryOperator::StrictInequality,
                            dep_expr.clone_in(allocator),
                        ),
                        builder.statement_block(SPAN, consequent),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(
                                    builder,
                                    cache_binding_name,
                                    *value_slot,
                                ),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::MultiDependencyMemoizedReturn {
            value_name,
            value_kind,
            deps,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut dep_assignments = builder.vec();
            let mut dep_guards = deps.iter().map(|(slot, dep_expr)| {
                let dep_expression = parse_expression_source(allocator, source_type, dep_expr).ok()?;
                dep_assignments.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    *slot,
                    dep_expression.clone_in(allocator),
                ));
                Some(builder.expression_binary(
                    SPAN,
                    cache_member_slot_expression(builder, cache_binding_name, *slot),
                    BinaryOperator::StrictInequality,
                    dep_expression,
                ))
            });
            let mut test = dep_guards.next()??;
            for guard in dep_guards {
                test = builder.expression_logical(SPAN, test, LogicalOperator::Or, guard?);
            }
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.extend(dep_assignments);
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(value_name),
                            ),
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    )),
                    builder.statement_if(
                        SPAN,
                        test,
                        builder.statement_block(SPAN, consequent),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(builder, cache_binding_name, *value_slot),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::MultiDependencyMemoizedExistingReturn {
            value_name,
            deps,
            value_slot,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_setup_statements,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let mut dep_assignments = builder.vec();
            let mut dep_guards = deps.iter().map(|(slot, dep_expr)| {
                let dep_expression = parse_expression_source(allocator, source_type, dep_expr).ok()?;
                dep_assignments.push(build_cache_slot_assignment_statement(
                    builder,
                    cache_binding_name,
                    *slot,
                    dep_expression.clone_in(allocator),
                ));
                Some(builder.expression_binary(
                    SPAN,
                    cache_member_slot_expression(builder, cache_binding_name, *slot),
                    BinaryOperator::StrictInequality,
                    dep_expression,
                ))
            });
            let mut test = dep_guards.next()??;
            for guard in dep_guards {
                test = builder.expression_logical(SPAN, test, LogicalOperator::Or, guard?);
            }
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.extend(build_generated_statement_sources(
                builder,
                allocator,
                source_type,
                memoized_setup_statements,
            )?);
            if let Some(memoized_expr) = memoized_expr {
                let memoized_expr =
                    parse_expression_source(allocator, source_type, memoized_expr).ok()?;
                consequent.push(build_identifier_assignment_statement(
                    builder,
                    value_name,
                    memoized_expr,
                ));
            }
            consequent.extend(dep_assignments);
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                *value_slot,
                builder.expression_identifier(SPAN, builder.ident(value_name)),
            ));
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    builder.statement_if(
                        SPAN,
                        test,
                        builder.statement_block(SPAN, consequent),
                        Some(builder.statement_block(
                            SPAN,
                            builder.vec1(build_identifier_assignment_statement(
                                builder,
                                value_name,
                                cache_member_slot_expression(
                                    builder,
                                    cache_binding_name,
                                    *value_slot,
                                ),
                            )),
                        )),
                    ),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::WrappedReturnExpression {
            expression,
            inner,
            ..
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let expression = parse_expression_source(allocator, source_type, expression).ok()?;
            replace_final_return_expression(&mut body, expression)?;
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AssignedAliasReturn {
            alias_name,
            source_name,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let last = body.statements.pop()?;
            let ast::Statement::ReturnStatement(return_stmt) = last else {
                return None;
            };
            let argument = return_stmt.argument.as_ref()?;
            let ast::Expression::Identifier(identifier) = argument.without_parentheses() else {
                return None;
            };
            if identifier.name.as_str() != source_name {
                return None;
            }
            body.statements.push(build_identifier_assignment_statement(
                builder,
                alias_name,
                builder.expression_identifier(SPAN, builder.ident(source_name)),
            ));
            body.statements.push(builder.statement_return(
                SPAN,
                Some(builder.expression_identifier(SPAN, builder.ident(alias_name))),
            ));
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AliasedReturn {
            alias_name,
            alias_kind,
            source_name,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let last = body.statements.pop()?;
            let ast::Statement::ReturnStatement(return_stmt) = last else {
                return None;
            };
            let argument = return_stmt.argument.as_ref()?;
            let ast::Expression::Identifier(identifier) = argument.without_parentheses() else {
                return None;
            };
            if identifier.name.as_str() != source_name {
                return None;
            }
            body.statements.push(ast::Statement::VariableDeclaration(
                builder.alloc_variable_declaration(
                    SPAN,
                    *alias_kind,
                    builder.vec1(builder.variable_declarator(
                        SPAN,
                        *alias_kind,
                        builder.binding_pattern_binding_identifier(
                            SPAN,
                            builder.ident(alias_name),
                        ),
                        NONE,
                        Some(builder.expression_identifier(SPAN, builder.ident(source_name))),
                        false,
                    )),
                    false,
                ),
            ));
            body.statements.push(builder.statement_return(
                SPAN,
                Some(builder.expression_identifier(SPAN, builder.ident(alias_name))),
            ));
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
            bindings,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let guard_aliases = collect_guard_alias_binding_map(bindings);
            if !guard_aliases.is_empty() {
                for statement in body.statements.iter_mut() {
                    rewrite_guard_aliases_in_statement_ast(
                        builder,
                        allocator,
                        source_type,
                        statement,
                        &guard_aliases,
                    );
                }
            }
            let mut prefixed =
                build_generated_binding_statements(builder, allocator, source_type, bindings)?;
            prefixed.extend(body.statements);
            body.statements = prefixed;
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedDeclarations {
            declarations,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let mut prefixed =
                build_generated_declaration_statements(builder, allocator, source_type, declarations)?;
            prefixed.extend(body.statements);
            body.statements = prefixed;
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedExpressionStatements {
            expressions,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let mut prefixed = build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                expressions,
            )?;
            prefixed.extend(body.statements);
            body.statements = prefixed;
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedAssignments {
            assignments,
            inner,
        } => {
            let mut body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            let mut prefixed =
                build_generated_assignment_statements(builder, allocator, source_type, assignments)?;
            prefixed.extend(body.statements);
            body.statements = prefixed;
            Some(body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Sequential {
            prefix,
            inner,
        } => {
            let mut prefix_body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                prefix.as_ref(),
                cache_prologue,
            )?;
            if matches!(prefix_body.statements.last(), Some(ast::Statement::ReturnStatement(_))) {
                prefix_body.statements.pop();
            }
            let inner_body = try_build_function_body_from_shape(
                builder,
                allocator,
                source_type,
                inner.as_ref(),
                cache_prologue,
            )?;
            prefix_body.statements.extend(inner_body.statements);
            Some(prefix_body)
        }
        crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleSlotMemoizedReturn {
            value_name,
            value_kind,
            temp_name,
            memoized_bindings,
            memoized_assignments,
            memoized_expressions,
            memoized_expr,
        } => {
            let cache_binding_name = &cache_prologue?.binding_name;
            let memo_name = "__memo";
            let memo_expr = parse_expression_source(allocator, source_type, memoized_expr).ok()?;
            let conditional = builder.expression_conditional(
                SPAN,
                builder.expression_binary(
                    SPAN,
                    builder.expression_identifier(SPAN, builder.ident(temp_name)),
                    BinaryOperator::StrictEquality,
                    builder.expression_identifier(SPAN, builder.ident("undefined")),
                ),
                builder.expression_identifier(SPAN, builder.ident(memo_name)),
                builder.expression_identifier(SPAN, builder.ident(temp_name)),
            );
            let mut consequent = build_generated_binding_statements(
                builder,
                allocator,
                source_type,
                memoized_bindings,
            )?;
            consequent.extend(build_generated_assignment_statements(
                builder,
                allocator,
                source_type,
                memoized_assignments,
            )?);
            consequent.extend(build_generated_expression_statements(
                builder,
                allocator,
                source_type,
                memoized_expressions,
            )?);
            consequent.push(build_identifier_assignment_statement(
                builder,
                memo_name,
                memo_expr,
            ));
            consequent.push(build_cache_slot_assignment_statement(
                builder,
                cache_binding_name,
                0,
                builder.expression_identifier(SPAN, builder.ident(memo_name)),
            ));
            let alternate = builder.vec1(build_identifier_assignment_statement(
                builder,
                memo_name,
                cache_member_slot_expression(builder, cache_binding_name, 0),
            ));
            let test = builder.expression_binary(
                SPAN,
                cache_member_slot_expression(builder, cache_binding_name, 0),
                BinaryOperator::StrictEquality,
                build_memo_cache_sentinel_expression(builder),
            );
            Some(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec_from_iter([
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            ast::VariableDeclarationKind::Let,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(memo_name),
                            ),
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    )),
                    builder.statement_if(
                        SPAN,
                        test,
                        builder.statement_block(SPAN, consequent),
                        Some(builder.statement_block(SPAN, alternate)),
                    ),
                    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
                        SPAN,
                        *value_kind,
                        builder.vec1(builder.variable_declarator(
                            SPAN,
                            *value_kind,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(value_name),
                            ),
                            NONE,
                            Some(conditional),
                            false,
                        )),
                        false,
                    )),
                    builder.statement_return(
                        SPAN,
                        Some(builder.expression_identifier(SPAN, builder.ident(value_name))),
                    ),
                ]),
            ))
        }
    }
}

pub(crate) fn normalize_compiled_body_for_hir_match(body_source: &str) -> String {
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(body_source);
    let iife_normalized = normalize_generated_body_iife_parenthesization(&flow_cast_normalized);
    if let Some(canonicalized) = canonicalize_body_source_for_hir_match(&iife_normalized) {
        return normalize_hir_match_destructuring_brace_spacing(
            &normalize_hir_match_object_shorthand_pairs(
                &normalize_hir_match_multiline_brace_literals(&canonicalized),
            ),
        );
    }
    let normalized = iife_normalized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    normalize_hir_match_destructuring_brace_spacing(&normalize_hir_match_object_shorthand_pairs(
        &normalize_hir_match_multiline_brace_literals(&normalized),
    ))
}

fn canonicalize_body_source_for_hir_match(body_source: &str) -> Option<String> {
    let allocator = Allocator::default();
    for source_type in [
        SourceType::mjs().with_jsx(true),
        SourceType::ts().with_jsx(true),
    ] {
        let Ok(statements) =
            parse_statements(&allocator, source_type, allocator.alloc_str(body_source))
        else {
            continue;
        };
        let builder = AstBuilder::new(&allocator);
        let program = builder.program(
            SPAN,
            source_type,
            "",
            builder.vec(),
            None,
            builder.vec(),
            statements,
        );
        return Some(codegen_program(&program).trim().to_string());
    }
    None
}

fn normalize_hir_match_multiline_brace_literals(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let is_bb_label_block = is_basic_block_label_open_brace(trimmed);
        let ends_with_open_brace = trimmed.ends_with('{');
        let is_obj_literal_start = (trimmed.ends_with("= {")
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
                let total_len: usize = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len();
                if total_len <= 200 {
                    result.push(
                        parts
                            .join(" ")
                            .replace("  ", " ")
                            .replace(", }", " }")
                            .replace(",}", " }"),
                    );
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

fn normalize_hir_match_object_shorthand_pairs(code: &str) -> String {
    code.lines()
        .map(|line| {
            let mut current = line.trim().to_string();
            loop {
                let next = collapse_hir_match_object_shorthand_pairs_once(&current);
                if next == current {
                    break current;
                }
                current = next;
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_hir_match_destructuring_brace_spacing(code: &str) -> String {
    code.lines()
        .map(collapse_hir_match_destructuring_brace_spacing)
        .collect::<Vec<_>>()
        .join("\n")
}

fn collapse_hir_match_destructuring_brace_spacing(line: &str) -> String {
    for prefix in ["const {", "let {", "var {"] {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        let Some((binding_part, suffix)) = rest.split_once("} =") else {
            continue;
        };
        return format!("{prefix}{} }} ={suffix}", binding_part.trim());
    }
    line.trim().to_string()
}

fn collapse_hir_match_object_shorthand_pairs_once(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '{' || ch == ',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let key_start = j;
            if key_start < bytes.len() && is_hir_match_ident_start(bytes[key_start] as char) {
                j += 1;
                while j < bytes.len() && is_hir_match_ident_continue(bytes[j] as char) {
                    j += 1;
                }
                let key = &line[key_start..j];
                let mut k = j;
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b':' {
                    k += 1;
                    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                        k += 1;
                    }
                    let value_start = k;
                    if value_start < bytes.len()
                        && is_hir_match_ident_start(bytes[value_start] as char)
                    {
                        k += 1;
                        while k < bytes.len() && is_hir_match_ident_continue(bytes[k] as char) {
                            k += 1;
                        }
                        let value = &line[value_start..k];
                        let mut suffix = k;
                        while suffix < bytes.len() && bytes[suffix].is_ascii_whitespace() {
                            suffix += 1;
                        }
                        if suffix < bytes.len()
                            && matches!(bytes[suffix], b',' | b'}')
                            && key == value
                        {
                            out.push(ch);
                            out.push_str(&line[i + 1..key_start]);
                            out.push_str(key);
                            if suffix > k {
                                out.push_str(&line[k..suffix]);
                            }
                            out.push(bytes[suffix] as char);
                            i = suffix + 1;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

fn is_hir_match_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_hir_match_ident_continue(ch: char) -> bool {
    is_hir_match_ident_start(ch) || ch.is_ascii_digit()
}

fn is_basic_block_label_open_brace(line: &str) -> bool {
    if !line.starts_with("bb") || !line.ends_with(": {") {
        return false;
    }
    let digits = &line[2..line.len() - 3];
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

fn parse_compiled_function_body<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
    body_source: &str,
) -> Result<ast::FunctionBody<'a>, String> {
    parse_rendered_function_body(
        allocator,
        source_type,
        cf.is_async,
        cf.is_generator,
        body_source,
    )
}

fn parse_rendered_function_body<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    is_async: bool,
    is_generator: bool,
    body_source: &str,
) -> Result<ast::FunctionBody<'a>, String> {
    let async_prefix = if is_async { "async " } else { "" };
    let generator_prefix = if is_generator { "*" } else { "" };
    let mut attempts = vec![(source_type, body_source.to_string())];
    let iife_normalized = normalize_generated_body_iife_parenthesization(body_source);
    if iife_normalized != body_source {
        attempts.push((source_type, iife_normalized.clone()));
    }
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(body_source);
    if flow_cast_normalized != body_source {
        let ts_source_type = source_type.with_typescript(true);
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
        let flow_iife_normalized =
            normalize_generated_body_iife_parenthesization(&flow_cast_normalized);
        if flow_iife_normalized != flow_cast_normalized {
            attempts.push((ts_source_type, flow_iife_normalized));
        }
    }

    let mut last_err = None;
    let mut statements = None;
    for (attempt_source_type, attempt_body) in attempts {
        let wrapper = format!(
            "{}function {}__codex_ast_body() {{\n{}\n}}",
            async_prefix, generator_prefix, attempt_body
        );
        match parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&wrapper),
        ) {
            Ok(parsed_statements) => {
                statements = Some(parsed_statements);
                break;
            }
            Err(err) => {
                last_err = Some((err, wrapper));
            }
        }
    }
    let mut statements = match statements {
        Some(statements) => statements,
        None => {
            let (err, _) = last_err.unwrap_or_else(|| {
                (
                    "failed to parse wrapped function body".to_string(),
                    String::new(),
                )
            });
            return Err(err);
        }
    };
    let statement = statements
        .pop()
        .ok_or_else(|| "failed to parse wrapped function body".to_string())?;
    let ast::Statement::FunctionDeclaration(function) = statement else {
        return Err("wrapped body did not parse as function declaration".to_string());
    };
    let function = function.unbox();
    function
        .body
        .map(|body| body.unbox())
        .ok_or_else(|| "wrapped function body missing body".to_string())
}

fn build_rendered_outlined_function_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    outlined: &RenderedOutlinedFunction,
    state: &AstRenderState,
) -> Option<ast::Statement<'a>> {
    let mut body = if let Some(body) = try_build_function_body_from_shape(
        builder,
        allocator,
        source_type,
        &outlined.body_shape,
        outlined.cache_prologue.as_ref(),
    ) {
        body
    } else {
        parse_rendered_function_body(
            allocator,
            source_type,
            outlined.is_async,
            outlined.is_generator,
            outlined.body.as_deref()?,
        )
        .ok()?
    };
    apply_preserved_directives(builder, &mut body, &outlined.directives);
    wrap_hook_guard_body(
        builder,
        allocator,
        &mut body,
        outlined.needs_function_hook_guard_wrapper,
        state,
    );
    prepend_cache_prologue_statements(
        builder,
        allocator,
        &mut body,
        outlined.cache_prologue.as_ref(),
        state,
    );
    let declaration = builder.declaration_function(
        SPAN,
        ast::FunctionType::FunctionDeclaration,
        Some(builder.binding_identifier(SPAN, builder.atom(&outlined.name))),
        outlined.is_generator,
        outlined.is_async,
        false,
        NONE,
        NONE,
        make_compiled_formal_params(
            builder,
            ast::FormalParameterKind::FormalParameter,
            &outlined.params,
        ),
        NONE,
        Some(builder.alloc(body)),
    );
    match declaration {
        ast::Declaration::FunctionDeclaration(function) => {
            Some(ast::Statement::FunctionDeclaration(function))
        }
        _ => None,
    }
}

fn parse_expression_source<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    expr_source: &str,
) -> Result<ast::Expression<'a>, String> {
    let ts_source_type = source_type.with_typescript(true);
    let mut attempts = vec![
        (source_type, expr_source.to_string()),
        (ts_source_type, expr_source.to_string()),
    ];
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(expr_source);
    if flow_cast_normalized != expr_source {
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
    }
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(expr_source);
    if flow_cast_rewritten != expr_source && flow_cast_rewritten != flow_cast_normalized {
        attempts.push((ts_source_type, flow_cast_rewritten));
    }
    for (attempt_source_type, attempt_expr) in attempts {
        let wrapped = format!("({attempt_expr});");
        let Ok(mut statements) = parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&wrapped),
        ) else {
            continue;
        };
        let Some(ast::Statement::ExpressionStatement(statement)) = statements.pop() else {
            continue;
        };
        let mut expression = statement.unbox().expression;
        loop {
            match expression {
                ast::Expression::ParenthesizedExpression(parenthesized)
                    if matches!(
                        parenthesized.expression.without_parentheses(),
                        ast::Expression::ArrowFunctionExpression(_)
                    ) =>
                {
                    expression = parenthesized.unbox().expression;
                }
                _ => break,
            }
        }
        return Ok(expression);
    }
    Err("failed to parse expression snippet".to_string())
}

fn parse_binding_pattern_source<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    pattern_source: &str,
) -> Result<ast::BindingPattern<'a>, String> {
    let ts_source_type = source_type.with_typescript(true);
    let mut attempts = vec![
        (source_type, pattern_source.to_string()),
        (ts_source_type, pattern_source.to_string()),
    ];
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(pattern_source);
    if flow_cast_normalized != pattern_source {
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
    }
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(pattern_source);
    if flow_cast_rewritten != pattern_source && flow_cast_rewritten != flow_cast_normalized {
        attempts.push((ts_source_type, flow_cast_rewritten));
    }
    for (attempt_source_type, attempt_pattern) in attempts {
        let wrapped = format!("const {attempt_pattern} = __codex_binding;");
        let Ok(mut statements) = parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&wrapped),
        ) else {
            continue;
        };
        let Some(ast::Statement::VariableDeclaration(declaration)) = statements.pop() else {
            continue;
        };
        let Some(declarator) = declaration.unbox().declarations.into_iter().next() else {
            continue;
        };
        return Ok(declarator.id);
    }
    Err("failed to parse binding pattern snippet".to_string())
}

fn parse_assignment_target_source<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    target_source: &str,
) -> Result<ast::AssignmentTarget<'a>, String> {
    let ts_source_type = source_type.with_typescript(true);
    let mut attempts = vec![
        (source_type, target_source.to_string()),
        (ts_source_type, target_source.to_string()),
    ];
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(target_source);
    if flow_cast_normalized != target_source {
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
    }
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(target_source);
    if flow_cast_rewritten != target_source && flow_cast_rewritten != flow_cast_normalized {
        attempts.push((ts_source_type, flow_cast_rewritten));
    }
    for (attempt_source_type, attempt_target) in attempts {
        let wrapped = format!("({attempt_target} = __codex_target);");
        let Ok(mut statements) = parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&wrapped),
        ) else {
            continue;
        };
        let Some(ast::Statement::ExpressionStatement(statement)) = statements.pop() else {
            continue;
        };
        let mut expression = statement.unbox().expression;
        while let ast::Expression::ParenthesizedExpression(parenthesized) = expression {
            expression = parenthesized.unbox().expression;
        }
        let ast::Expression::AssignmentExpression(assignment) = expression else {
            continue;
        };
        return Ok(assignment.unbox().left);
    }
    Err("failed to parse assignment target snippet".to_string())
}

fn normalize_generated_body_iife_parenthesization(body_source: &str) -> String {
    let mut changed = false;
    let normalized = body_source
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let indent = &line[..line.len() - line.trim_start().len()];
            match trimmed {
                "function() {" => {
                    changed = true;
                    format!("{indent}(function() {{")
                }
                "function () {" => {
                    changed = true;
                    format!("{indent}(function () {{")
                }
                "}();" => {
                    changed = true;
                    format!("{indent}}})();")
                }
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if changed {
        normalized
    } else {
        body_source.to_string()
    }
}

fn normalize_generated_body_flow_cast_marker_calls(body_source: &str) -> String {
    let rewritten = crate::pipeline::rewrite_flow_cast_expressions(body_source);
    if rewritten == body_source {
        return body_source.to_string();
    }
    rewrite_flow_cast_marker_calls(&rewritten)
}

fn rewrite_flow_cast_marker_calls(source: &str) -> String {
    let mut changed = false;
    let mut out = String::with_capacity(source.len());
    let mut paren_stack: Vec<usize> = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if source[i..].starts_with("//") {
            while i < bytes.len() {
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                if ch == '\n' {
                    break;
                }
            }
            continue;
        }
        if source[i..].starts_with("/*") {
            out.push('/');
            out.push('*');
            i += 2;
            while i < bytes.len() {
                if source[i..].starts_with("*/") {
                    out.push('*');
                    out.push('/');
                    i += 2;
                    break;
                }
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }

        let ch = source[i..].chars().next().unwrap();
        if ch == '\'' || ch == '"' || ch == '`' {
            let quote = ch;
            out.push(ch);
            i += ch.len_utf8();
            let mut escaped = false;
            while i < bytes.len() {
                let c = source[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
                if escaped {
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == quote {
                    break;
                }
            }
            continue;
        }

        if ch == '(' {
            paren_stack.push(out.len());
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if ch == ')' {
            out.push(ch);
            i += ch.len_utf8();

            if let Some(open_idx) = paren_stack.pop() {
                let close_idx = out.len() - 1;
                if open_idx < close_idx {
                    let inner = &out[open_idx + 1..close_idx];
                    if let Some((expr, ty)) = split_flow_cast_marker_inner(inner) {
                        let replacement =
                            format!("{FLOW_CAST_MARKER_HELPER}<{}>({})", ty.trim(), expr.trim());
                        out.replace_range(open_idx..=close_idx, &replacement);
                        changed = true;
                    }
                }
            }
            continue;
        }

        out.push(ch);
        i += ch.len_utf8();
    }

    if changed { out } else { source.to_string() }
}

fn split_flow_cast_marker_inner(inner: &str) -> Option<(String, String)> {
    const MARKER: &str = " as /*__FLOW_CAST__*/ ";
    let chars: Vec<(usize, char)> = inner.char_indices().collect();
    let mut depth_paren = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    let mut depth_angle = 0usize;

    for (byte_idx, ch) in chars {
        match ch {
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            '<' => depth_angle += 1,
            '>' => depth_angle = depth_angle.saturating_sub(1),
            _ => {}
        }
        let at_top = depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 && depth_angle == 0;
        if at_top && inner[byte_idx..].starts_with(MARKER) {
            let left = inner[..byte_idx].trim();
            let right = inner[byte_idx + MARKER.len()..].trim();
            if left.is_empty() || right.is_empty() {
                return None;
            }
            return Some((left.to_string(), right.to_string()));
        }
    }

    None
}

fn restore_flow_cast_marker_calls(source: &str) -> String {
    if !source.contains(FLOW_CAST_MARKER_HELPER) {
        return source.to_string();
    }

    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if starts_flow_cast_marker(source, i)
            && let Some((replacement, next_idx)) = parse_flow_cast_marker_call(source, i)
        {
            out.push_str(&replacement);
            i = next_idx;
            continue;
        }

        let ch = source[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn starts_flow_cast_marker(source: &str, idx: usize) -> bool {
    if !source[idx..].starts_with(FLOW_CAST_MARKER_HELPER) {
        return false;
    }
    let prev = source[..idx].chars().next_back();
    !prev.is_some_and(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn parse_flow_cast_marker_call(source: &str, idx: usize) -> Option<(String, usize)> {
    let mut i = idx + FLOW_CAST_MARKER_HELPER.len();
    i = skip_ascii_whitespace(source, i);
    if source[i..].chars().next()? != '<' {
        return None;
    }
    let (type_annotation, after_type) = parse_balanced_angle_contents(source, i)?;
    let i = skip_ascii_whitespace(source, after_type);
    if source[i..].chars().next()? != '(' {
        return None;
    }
    let (arg, after_arg) = parse_balanced_paren_contents(source, i)?;
    let restored_arg = restore_flow_cast_marker_calls(arg.trim());
    Some((
        format!("({}: {})", restored_arg.trim(), type_annotation.trim()),
        after_arg,
    ))
}

fn skip_ascii_whitespace(source: &str, mut idx: usize) -> usize {
    while idx < source.len() {
        let ch = source[idx..].chars().next().unwrap();
        if !ch.is_ascii_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

fn parse_balanced_angle_contents(source: &str, open_idx: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut i = open_idx + 1;
    let mut depth_angle = 1usize;
    let mut depth_paren = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    while i < bytes.len() {
        let ch = source[i..].chars().next().unwrap();
        match ch {
            '\'' | '"' | '`' => {
                i = skip_quoted(source, i)?;
                continue;
            }
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            '<' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                depth_angle += 1;
            }
            '>' if depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 => {
                depth_angle = depth_angle.saturating_sub(1);
                if depth_angle == 0 {
                    return Some((source[open_idx + 1..i].to_string(), i + 1));
                }
            }
            _ => {}
        }
        i += ch.len_utf8();
    }
    None
}

fn parse_balanced_paren_contents(source: &str, open_idx: usize) -> Option<(String, usize)> {
    let bytes = source.as_bytes();
    let mut i = open_idx + 1;
    let mut depth_paren = 1usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    while i < bytes.len() {
        let ch = source[i..].chars().next().unwrap();
        match ch {
            '\'' | '"' | '`' => {
                i = skip_quoted(source, i)?;
                continue;
            }
            '(' => depth_paren += 1,
            ')' => {
                depth_paren = depth_paren.saturating_sub(1);
                if depth_paren == 0 {
                    return Some((source[open_idx + 1..i].to_string(), i + 1));
                }
            }
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            _ => {}
        }
        i += ch.len_utf8();
    }
    None
}

fn skip_quoted(source: &str, start_idx: usize) -> Option<usize> {
    let quote = source[start_idx..].chars().next()?;
    let bytes = source.as_bytes();
    let mut i = start_idx + quote.len_utf8();
    let mut escaped = false;
    while i < bytes.len() {
        let ch = source[i..].chars().next().unwrap();
        i += ch.len_utf8();
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(i);
        }
    }
    None
}

fn apply_preserved_directives<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    directives: &[String],
) {
    if directives.is_empty() {
        return;
    }
    body.directives = builder.vec_from_iter(
        directives
            .iter()
            .filter_map(|directive| build_directive(builder, directive)),
    );
}

fn build_directive<'a>(builder: AstBuilder<'a>, directive: &str) -> Option<ast::Directive<'a>> {
    let value = parse_directive_literal_value(directive)?;
    Some(builder.directive(
        SPAN,
        builder.string_literal(SPAN, builder.atom(value), None),
        builder.atom(value),
    ))
}

fn parse_directive_literal_value(directive: &str) -> Option<&str> {
    let directive = directive.trim();
    if directive.len() < 2 {
        return None;
    }
    let quote = directive.chars().next()?;
    if !matches!(quote, '"' | '\'') || !directive.ends_with(quote) {
        return None;
    }
    Some(&directive[quote.len_utf8()..directive.len() - quote.len_utf8()])
}

fn prepend_cache_prologue_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cache_prologue: Option<&crate::reactive_scopes::codegen_reactive::CachePrologue>,
    state: &AstRenderState,
) {
    let Some(cache_prologue) = cache_prologue else {
        return;
    };

    let mut statements = builder.vec1(build_cache_initializer_statement(
        builder,
        &cache_prologue.binding_name,
        cache_prologue.size,
        &state.cache_import_name,
    ));
    if let Some(fast_refresh) = cache_prologue.fast_refresh.as_ref() {
        statements.push(build_fast_refresh_reset_statement(
            builder,
            &cache_prologue.binding_name,
            cache_prologue.size,
            fast_refresh,
        ));
    }
    statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
}

fn prepend_synthesized_default_param_cache_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
) -> Option<()> {
    let Some(default_cache) = cf.synthesized_default_param_cache.as_ref() else {
        return Some(());
    };
    let cache_binding_name = cf
        .cache_prologue
        .as_ref()
        .map(|cache_prologue| cache_prologue.binding_name.as_str())
        .unwrap_or("$");
    let insert_idx = cf.cache_prologue.as_ref().map_or(0, |cache_prologue| {
        1 + usize::from(cache_prologue.fast_refresh.is_some())
    });
    let mut statements = builder.vec();
    statements.extend(
        body.statements[..insert_idx]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    statements.extend(build_synthesized_default_param_cache_statements(
        builder,
        allocator,
        source_type,
        cache_binding_name,
        default_cache,
    )?);
    statements.extend(
        body.statements[insert_idx..]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
    Some(())
}

fn build_synthesized_default_param_cache_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cache_binding_name: &str,
    default_cache: &SynthesizedDefaultParamCache,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let value_expr =
        parse_expression_source(allocator, source_type, &default_cache.value_expr).ok()?;
    let undefined_check = builder.expression_binary(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
        BinaryOperator::StrictEquality,
        builder.expression_identifier(SPAN, builder.ident("undefined")),
    );
    let assign_value = build_identifier_assignment_statement(
        builder,
        &default_cache.value_name,
        builder.expression_conditional(
            SPAN,
            undefined_check,
            value_expr,
            builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
        ),
    );
    let else_block = builder.statement_block(
        SPAN,
        builder.vec1(build_identifier_assignment_statement(
            builder,
            &default_cache.value_name,
            cache_member_slot_expression(builder, cache_binding_name, 1),
        )),
    );

    Some(builder.vec_from_array([
        build_let_declaration_statement(builder, &default_cache.value_name),
        builder.statement_if(
            SPAN,
            builder.expression_binary(
                SPAN,
                cache_member_slot_expression(builder, cache_binding_name, 0),
                BinaryOperator::StrictInequality,
                builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
            ),
            builder.statement_block(
                SPAN,
                builder.vec_from_array(
                    [
                        assign_value,
                        build_cache_slot_assignment_statement(
                            builder,
                            cache_binding_name,
                            0,
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&default_cache.temp_name),
                            ),
                        ),
                        build_cache_slot_assignment_statement(
                            builder,
                            cache_binding_name,
                            1,
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&default_cache.value_name),
                            ),
                        ),
                    ],
                ),
            ),
            Some(else_block),
        ),
    ]))
}

fn wrap_function_hook_guard_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    wrap_hook_guard_body(
        builder,
        allocator,
        body,
        cf.needs_function_hook_guard_wrapper,
        state,
    );
}

fn wrap_hook_guard_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    needs_function_hook_guard_wrapper: bool,
    state: &AstRenderState,
) {
    if !needs_function_hook_guard_wrapper || state.hook_guard_ident.is_empty() {
        return;
    }

    let mut try_statements = builder.vec1(build_hook_guard_statement(
        builder,
        &state.hook_guard_ident,
        crate::reactive_scopes::codegen_reactive::HOOK_GUARD_PUSH,
    ));
    try_statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    let finalizer = builder.alloc_block_statement(
        SPAN,
        builder.vec1(build_hook_guard_statement(
            builder,
            &state.hook_guard_ident,
            crate::reactive_scopes::codegen_reactive::HOOK_GUARD_POP,
        )),
    );

    body.statements = builder.vec1(ast::Statement::TryStatement(builder.alloc_try_statement(
        SPAN,
        builder.alloc_block_statement(SPAN, try_statements),
        None::<oxc_allocator::Box<'a, ast::CatchClause<'a>>>,
        Some(finalizer),
    )));
}

fn build_hook_guard_statement<'a>(
    builder: AstBuilder<'a>,
    hook_guard_ident: &str,
    action: u8,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(hook_guard_ident)),
            NONE,
            builder.vec1(ast::Argument::from(builder.expression_numeric_literal(
                SPAN,
                action as f64,
                None,
                NumberBase::Decimal,
            ))),
            false,
        ),
    )
}

fn build_let_declaration_statement<'a>(builder: AstBuilder<'a>, name: &str) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(SPAN, builder.ident(name));
    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        SPAN,
        ast::VariableDeclarationKind::Let,
        builder.vec1(builder.variable_declarator(
            SPAN,
            ast::VariableDeclarationKind::Let,
            pattern,
            NONE,
            None,
            false,
        )),
        false,
    ))
}

fn build_identifier_assignment_statement<'a>(
    builder: AstBuilder<'a>,
    name: &str,
    value: ast::Expression<'a>,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(
                builder.simple_assignment_target_assignment_target_identifier(
                    SPAN,
                    builder.ident(name),
                ),
            ),
            value,
        ),
    )
}

fn build_cache_slot_assignment_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    slot: u32,
    value: ast::Expression<'a>,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                builder.member_expression_computed(
                    SPAN,
                    builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                    builder.expression_numeric_literal(
                        SPAN,
                        slot as f64,
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            value,
        ),
    )
}

fn normalize_use_fire_binding_temps_ast<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
) {
    if !cf.normalize_use_fire_binding_temps {
        return;
    }

    let mut declared_names = Vec::new();
    let mut collector = UseFireBindingCollector {
        declared_names: &mut declared_names,
    };
    collector.visit_function_body(body);
    if declared_names.len() < 2 {
        return;
    }

    let mut desired = declared_names.clone();
    desired.sort_by_key(|name| parse_temp_token_index(name).unwrap_or(u32::MAX));
    if desired == declared_names {
        return;
    }

    let renames = declared_names
        .iter()
        .zip(desired.iter())
        .filter_map(|(from, to)| (from != to).then_some((from.as_str(), to.as_str())))
        .collect::<Vec<_>>();
    if renames.is_empty() {
        return;
    }

    let mut renamer = BindingAndReferenceRenamer { builder, renames };
    renamer.visit_function_body(body);
}

fn parse_temp_token_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('t')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

fn prepend_compiled_body_prefix_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    original_body: Option<&ast::FunctionBody<'_>>,
    cache_import_name: Option<&str>,
) -> Option<()> {
    let prefix_statements =
        collect_compiled_body_prefix_statements(allocator, source_type, body, cf)?;
    let preserved_original_statements =
        collect_preserved_original_body_statements(allocator, original_body);
    if prefix_statements.is_empty() && preserved_original_statements.is_empty() {
        return Some(());
    }
    let insert_idx = cache_import_name
        .and_then(|cache_import_name| {
            find_cache_initializer_index(&body.statements, cache_import_name)
        })
        .map_or(0, |index| index + 1);
    let mut statements = builder.vec();
    statements.extend(
        body.statements[..insert_idx]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    statements.extend(prefix_statements);
    statements.extend(preserved_original_statements);
    statements.extend(
        body.statements[insert_idx..]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
    Some(())
}

fn build_cache_initializer_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    cache_size: u32,
    cache_import_name: &str,
) -> ast::Statement<'a> {
    build_const_binding_statement(
        builder,
        SPAN,
        cache_binding_name,
        builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(cache_import_name)),
            NONE,
            builder.vec1(ast::Argument::from(builder.expression_numeric_literal(
                SPAN,
                cache_size as f64,
                None,
                NumberBase::Decimal,
            ))),
            false,
        ),
    )
}

fn build_fast_refresh_reset_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    cache_size: u32,
    fast_refresh: &crate::reactive_scopes::codegen_reactive::FastRefreshPrologue,
) -> ast::Statement<'a> {
    let test = builder.expression_binary(
        SPAN,
        cache_member_slot_expression(builder, cache_binding_name, fast_refresh.cache_index),
        BinaryOperator::StrictInequality,
        builder.expression_string_literal(SPAN, builder.atom(&fast_refresh.hash), None),
    );
    let mut consequent_statements = builder.vec1(builder.statement_for(
        SPAN,
        Some(ast::ForStatementInit::VariableDeclaration(
            builder.alloc_variable_declaration(
                SPAN,
                ast::VariableDeclarationKind::Let,
                builder.vec1(builder.variable_declarator(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    builder.binding_pattern_binding_identifier(
                        SPAN,
                        builder.ident(&fast_refresh.index_binding_name),
                    ),
                    NONE,
                    Some(builder.expression_numeric_literal(SPAN, 0.0, None, NumberBase::Decimal)),
                    false,
                )),
                false,
            ),
        )),
        Some(builder.expression_binary(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(&fast_refresh.index_binding_name)),
            BinaryOperator::LessThan,
            builder.expression_numeric_literal(SPAN, cache_size as f64, None, NumberBase::Decimal),
        )),
        Some(builder.expression_assignment(
            SPAN,
            AssignmentOperator::Addition,
            ast::AssignmentTarget::from(
                builder.simple_assignment_target_assignment_target_identifier(
                    SPAN,
                    builder.ident(&fast_refresh.index_binding_name),
                ),
            ),
            builder.expression_numeric_literal(SPAN, 1.0, None, NumberBase::Decimal),
        )),
        builder.statement_block(
            SPAN,
            builder.vec1(builder.statement_expression(
                SPAN,
                builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                        builder.member_expression_computed(
                            SPAN,
                            builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&fast_refresh.index_binding_name),
                            ),
                            false,
                        ),
                    )),
                    build_memo_cache_sentinel_expression(builder),
                ),
            )),
        ),
    ));
    consequent_statements.push(builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                builder.member_expression_computed(
                    SPAN,
                    builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                    builder.expression_numeric_literal(
                        SPAN,
                        fast_refresh.cache_index as f64,
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            builder.expression_string_literal(SPAN, builder.atom(&fast_refresh.hash), None),
        ),
    ));
    builder.statement_if(
        SPAN,
        test,
        builder.statement_block(SPAN, consequent_statements),
        None,
    )
}

fn cache_member_slot_expression<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    slot: u32,
) -> ast::Expression<'a> {
    ast::Expression::from(builder.member_expression_computed(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
        builder.expression_numeric_literal(SPAN, slot as f64, None, NumberBase::Decimal),
        false,
    ))
}

fn build_memo_cache_sentinel_expression<'a>(builder: AstBuilder<'a>) -> ast::Expression<'a> {
    builder.expression_call(
        SPAN,
        ast::Expression::from(builder.member_expression_static(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident("Symbol")),
            builder.identifier_name(SPAN, "for"),
            false,
        )),
        NONE,
        builder.vec1(ast::Argument::from(builder.expression_string_literal(
            SPAN,
            builder.atom(crate::reactive_scopes::codegen_reactive::MEMO_CACHE_SENTINEL),
            None,
        ))),
        false,
    )
}

fn build_early_return_sentinel_expression<'a>(builder: AstBuilder<'a>) -> ast::Expression<'a> {
    builder.expression_call(
        SPAN,
        ast::Expression::from(builder.member_expression_static(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident("Symbol")),
            builder.identifier_name(SPAN, "for"),
            false,
        )),
        NONE,
        builder.vec1(ast::Argument::from(builder.expression_string_literal(
            SPAN,
            builder.atom(crate::reactive_scopes::codegen_reactive::EARLY_RETURN_SENTINEL),
            None,
        ))),
        false,
    )
}

fn prepend_instrument_forget_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    if !cf.needs_instrument_forget {
        return;
    }

    let rendered_name = if cf.name.is_empty() {
        "<anonymous>"
    } else {
        cf.name.as_str()
    };
    let test = builder.expression_logical(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident("DEV")),
        LogicalOperator::And,
        builder.expression_identifier(SPAN, builder.ident(&state.should_instrument_ident)),
    );
    let call = builder.expression_call(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(&state.use_render_counter_ident)),
        NONE,
        builder.vec_from_iter([
            ast::Argument::from(builder.expression_string_literal(
                SPAN,
                builder.atom(rendered_name),
                None,
            )),
            ast::Argument::from(builder.expression_string_literal(
                SPAN,
                builder.atom(&state.instrument_source_path),
                None,
            )),
        ]),
        false,
    );
    let statement =
        builder.statement_if(SPAN, test, builder.statement_expression(SPAN, call), None);

    let mut statements = builder.vec1(statement);
    statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
}

fn collect_rendered_outlined_functions(cf: &CompiledFunction) -> Vec<RenderedOutlinedFunction> {
    cf.outlined_functions
        .iter()
        .map(|outlined_function| RenderedOutlinedFunction {
            name: outlined_function.name.clone(),
            params: outlined_function.params.clone(),
            body: outlined_function.body.clone(),
            body_shape: outlined_function.body_shape.clone(),
            directives: outlined_function.directives.clone(),
            cache_prologue: outlined_function.cache_prologue.clone(),
            needs_function_hook_guard_wrapper: outlined_function.needs_function_hook_guard_wrapper,
            is_async: outlined_function.is_async,
            is_generator: outlined_function.is_generator,
        })
        .collect()
}

fn try_lower_compiled_statement_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if !can_emit_compiled_statement_ast(cf) {
        return None;
    }
    let mut statements = builder.vec();
    let mut function_statement =
        super::hir_to_ast::try_lower_function_declaration_ast(builder, cf.hir_function.as_ref()?)?;
    let ast::Statement::FunctionDeclaration(function) = &mut function_statement else {
        return None;
    };
    prepend_hir_body_prefix_statements(builder, allocator, source_type, function, cf)?;
    prepend_hir_instrument_forget_statement(builder, allocator, function, cf, state)?;
    apply_emit_freeze_to_hir_function_body(builder, allocator, function, cf, state)?;
    statements.push(function_statement);
    if !cf.outlined_functions.is_empty() {
        return None;
    }
    for (_, hir_function) in &cf.hir_outlined_functions {
        statements.push(super::hir_to_ast::try_lower_function_declaration_ast(
            builder,
            hir_function,
        )?);
    }
    Some(statements)
}

fn prepend_hir_body_prefix_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
) -> Option<()> {
    let body = function.body.as_ref()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    prepend_compiled_body_prefix_statements(
        builder,
        allocator,
        source_type,
        &mut cloned_body,
        cf,
        None,
        None,
    )?;
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

fn prepend_hir_instrument_forget_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<()> {
    let body = function.body.as_mut()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    prepend_instrument_forget_statement(builder, allocator, &mut cloned_body, cf, state);
    align_runtime_identifier_references(builder, &mut cloned_body, cf, state);
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

fn apply_emit_freeze_to_hir_function_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<()> {
    let body = function.body.as_mut()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    apply_emit_freeze_to_cache_stores_ast(builder, allocator, &mut cloned_body, cf, state);
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

fn align_runtime_identifier_references<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    let mut renames = Vec::new();
    if cf.needs_hook_guards && !state.hook_guard_ident.is_empty() {
        renames.push(("$dispatcherGuard", state.hook_guard_ident.as_str()));
    }
    if cf.needs_structural_check_import && !state.structural_check_ident.is_empty() {
        renames.push(("$structuralCheck", state.structural_check_ident.as_str()));
    }
    if cf.needs_lower_context_access
        && !state.lower_context_access_imported.is_empty()
        && !state.lower_context_access_ident.is_empty()
        && state.lower_context_access_imported != state.lower_context_access_ident
    {
        renames.push((
            state.lower_context_access_imported.as_str(),
            state.lower_context_access_ident.as_str(),
        ));
    }
    let cache_import_name =
        (state.cache_import_name != "_c").then_some(state.cache_import_name.as_str());
    if renames.is_empty() && cache_import_name.is_none() {
        return;
    }

    let mut renamer = IdentifierReferenceRenamer {
        builder,
        cache_import_name,
        renames,
    };
    renamer.visit_function_body(body);
}

fn apply_emit_freeze_to_cache_stores_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    if !cf.needs_emit_freeze || state.make_read_only_ident.is_empty() || cf.name.is_empty() {
        return;
    }

    let mut collector = OutputCacheSlotCollector {
        output_slots: HashSet::new(),
    };
    collector.visit_function_body(body);
    if collector.output_slots.is_empty() {
        return;
    }

    let mut rewriter = EmitFreezeCacheStoreRewriter {
        builder,
        allocator,
        freeze_ident: state.make_read_only_ident.as_str(),
        function_name: cf.name.as_str(),
        output_slots: collector.output_slots,
    };
    rewriter.visit_function_body(body);
}

struct IdentifierReferenceRenamer<'a, 'rename> {
    builder: AstBuilder<'a>,
    cache_import_name: Option<&'rename str>,
    renames: Vec<(&'rename str, &'rename str)>,
}

struct BindingAndReferenceRenamer<'a, 'rename> {
    builder: AstBuilder<'a>,
    renames: Vec<(&'rename str, &'rename str)>,
}

struct OutputCacheSlotCollector {
    output_slots: HashSet<(String, u32)>,
}

struct UseFireBindingCollector<'a> {
    declared_names: &'a mut Vec<String>,
}

struct EmitFreezeCacheStoreRewriter<'a, 'rename> {
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    freeze_ident: &'rename str,
    function_name: &'rename str,
    output_slots: HashSet<(String, u32)>,
}

impl<'a> VisitMut<'a> for IdentifierReferenceRenamer<'a, '_> {
    fn visit_call_expression(&mut self, it: &mut ast::CallExpression<'a>) {
        if let Some(cache_import_name) = self.cache_import_name
            && let ast::Expression::Identifier(identifier) = &mut it.callee
            && identifier.name == "_c"
        {
            identifier.name = self.builder.ident(cache_import_name);
        }
        walk_mut::walk_call_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &mut ast::IdentifierReference<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }
}

impl<'a> VisitMut<'a> for BindingAndReferenceRenamer<'a, '_> {
    fn visit_binding_identifier(&mut self, it: &mut ast::BindingIdentifier<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }

    fn visit_identifier_reference(&mut self, it: &mut ast::IdentifierReference<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }
}

impl<'a> Visit<'a> for UseFireBindingCollector<'_> {
    fn visit_variable_declarator(&mut self, it: &ast::VariableDeclarator<'a>) {
        if let Some(ast::Expression::CallExpression(call)) = &it.init
            && matches!(&call.callee, ast::Expression::Identifier(identifier) if identifier.name == "useFire")
            && let ast::BindingPattern::BindingIdentifier(ident) = &it.id
            && parse_temp_token_index(ident.name.as_str()).is_some()
            && !self
                .declared_names
                .iter()
                .any(|existing| existing == ident.name.as_str())
        {
            self.declared_names.push(ident.name.to_string());
        }
        walk::walk_variable_declarator(self, it);
    }
}

impl<'a> Visit<'a> for OutputCacheSlotCollector {
    fn visit_assignment_expression(&mut self, it: &ast::AssignmentExpression<'a>) {
        if it.operator == AssignmentOperator::Assign
            && let Some(_) = assignment_target_identifier_name(&it.left)
            && let Some((cache_name, slot)) = expression_cache_access(&it.right)
        {
            self.output_slots.insert((cache_name.to_string(), slot));
        }
        walk::walk_assignment_expression(self, it);
    }
}

impl<'a> VisitMut<'a> for EmitFreezeCacheStoreRewriter<'a, '_> {
    fn visit_assignment_expression(&mut self, it: &mut ast::AssignmentExpression<'a>) {
        walk_mut::walk_assignment_expression(self, it);
        if it.operator != AssignmentOperator::Assign {
            return;
        }
        let Some((cache_name, slot)) = assignment_target_cache_access(&it.left) else {
            return;
        };
        if !self.output_slots.contains(&(cache_name.to_string(), slot))
            || expression_references_identifier(&it.right, self.freeze_ident)
        {
            return;
        }

        let original_right = it.right.clone_in(self.allocator);
        let freeze_call = self.builder.expression_call(
            SPAN,
            self.builder
                .expression_identifier(SPAN, self.builder.ident(self.freeze_ident)),
            NONE,
            self.builder.vec_from_iter([
                ast::Argument::from(original_right.clone_in(self.allocator)),
                ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(self.function_name),
                    None,
                )),
            ]),
            false,
        );
        it.right = self.builder.expression_conditional(
            SPAN,
            self.builder
                .expression_identifier(SPAN, self.builder.ident("__DEV__")),
            freeze_call,
            original_right,
        );
    }
}

fn assignment_target_identifier_name<'a>(target: &'a ast::AssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        ast::AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
            Some(identifier.name.as_str())
        }
        _ => None,
    }
}

fn assignment_target_cache_access<'a>(
    target: &'a ast::AssignmentTarget<'a>,
) -> Option<(&'a str, u32)> {
    match target {
        ast::AssignmentTarget::ComputedMemberExpression(member) => member_cache_access(member),
        _ => None,
    }
}

fn expression_cache_access<'a>(expression: &'a ast::Expression<'a>) -> Option<(&'a str, u32)> {
    match expression {
        ast::Expression::ComputedMemberExpression(member) => member_cache_access(member),
        _ => None,
    }
}

fn member_cache_access<'a>(
    member: &'a ast::ComputedMemberExpression<'a>,
) -> Option<(&'a str, u32)> {
    let ast::Expression::Identifier(object) = &member.object else {
        return None;
    };
    let ast::Expression::NumericLiteral(slot) = &member.expression else {
        return None;
    };
    let value = slot.value;
    if value.fract() != 0.0 || value < 0.0 || value > u32::MAX as f64 {
        return None;
    }
    Some((object.name.as_str(), value as u32))
}

fn expression_references_identifier(expression: &ast::Expression<'_>, ident: &str) -> bool {
    let mut detector = IdentifierReferenceDetector {
        ident,
        found: false,
    };
    detector.visit_expression(expression);
    detector.found
}

struct IdentifierReferenceDetector<'ident> {
    ident: &'ident str,
    found: bool,
}

impl<'a> Visit<'a> for IdentifierReferenceDetector<'_> {
    fn visit_identifier_reference(&mut self, it: &ast::IdentifierReference<'a>) {
        if it.name == self.ident {
            self.found = true;
        }
    }
}

fn function_body_contains_undefined_fallback(
    statements: &oxc_allocator::Vec<'_, ast::Statement<'_>>,
) -> bool {
    let mut detector = UndefinedFallbackDetector { found: false };
    detector.visit_statements(statements);
    detector.found
}

struct UndefinedFallbackDetector {
    found: bool,
}

impl<'a> Visit<'a> for UndefinedFallbackDetector {
    fn visit_expression(&mut self, expression: &ast::Expression<'a>) {
        if self.found {
            return;
        }
        if let ast::Expression::ConditionalExpression(conditional) = expression
            && conditional_expression_is_undefined_fallback(conditional)
        {
            self.found = true;
            return;
        }
        walk::walk_expression(self, expression);
    }
}

fn conditional_expression_is_undefined_fallback(
    conditional: &ast::ConditionalExpression<'_>,
) -> bool {
    fn compared_identifier_name<'a>(expression: &'a ast::Expression<'a>) -> Option<&'a str> {
        let ast::Expression::BinaryExpression(binary) = expression.without_parentheses() else {
            return None;
        };
        if binary.operator != BinaryOperator::StrictEquality {
            return None;
        }
        if expression_is_undefined(&binary.right) {
            return identifier_reference_name(&binary.left);
        }
        if expression_is_undefined(&binary.left) {
            return identifier_reference_name(&binary.right);
        }
        None
    }

    fn identifier_reference_name<'a>(expression: &'a ast::Expression<'a>) -> Option<&'a str> {
        let ast::Expression::Identifier(identifier) = expression.without_parentheses() else {
            return None;
        };
        Some(identifier.name.as_str())
    }

    fn expression_is_undefined(expression: &ast::Expression<'_>) -> bool {
        matches!(
            expression.without_parentheses(),
            ast::Expression::Identifier(identifier) if identifier.name == "undefined"
        )
    }

    let Some(ident) = compared_identifier_name(&conditional.test) else {
        return false;
    };
    expression_references_identifier(&conditional.consequent, ident)
        || expression_references_identifier(&conditional.alternate, ident)
}

fn collect_compiled_body_prefix_statements<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &ast::FunctionBody<'_>,
    cf: &CompiledFunction,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let builder = AstBuilder::new(allocator);
    let mut statements = oxc_allocator::Vec::new_in(allocator);
    if cf.param_prefix_statements.is_empty() {
        return Some(statements);
    }
    if function_body_contains_undefined_fallback(&body.statements) {
        return Some(statements);
    }
    let body_identifier_references = Some(collect_identifier_references_in_statements(
        &body.statements,
    ));
    for (index, statement) in cf.param_prefix_statements.iter().enumerate() {
        let later_statements = &cf.param_prefix_statements[index + 1..];
        let Some(pruned) = prune_compiled_prefix_statement(
            allocator,
            source_type,
            statement,
            body_identifier_references.as_ref(),
            later_statements,
        ) else {
            continue;
        };
        statements.push(build_compiled_prefix_statement(
            builder,
            allocator,
            source_type,
            &pruned,
        )?);
    }
    Some(statements)
}

fn prune_compiled_prefix_statement(
    allocator: &Allocator,
    source_type: SourceType,
    statement: &CompiledParamPrefixStatement,
    body_identifier_references: Option<&HashSet<String>>,
    later_statements: &[CompiledParamPrefixStatement],
) -> Option<CompiledParamPrefixStatement> {
    let CompiledBindingPattern::Object(pattern) = &statement.pattern else {
        return Some(statement.clone());
    };

    let mut properties = Vec::new();
    for property in &pattern.properties {
        if compiled_binding_pattern_is_used(
            allocator,
            source_type,
            &property.value,
            body_identifier_references,
            later_statements,
        ) {
            properties.push(property.clone());
        }
    }

    let rest = pattern.rest.as_ref().and_then(|rest| {
        compiled_binding_pattern_is_used(
            allocator,
            source_type,
            rest,
            body_identifier_references,
            later_statements,
        )
        .then(|| rest.clone())
    });

    if properties.is_empty() && rest.is_none() {
        return None;
    }

    Some(CompiledParamPrefixStatement {
        kind: statement.kind,
        pattern: CompiledBindingPattern::Object(CompiledObjectPattern { properties, rest }),
        init: statement.init.clone(),
    })
}

fn compiled_binding_pattern_is_used(
    allocator: &Allocator,
    source_type: SourceType,
    pattern: &CompiledBindingPattern,
    body_identifier_references: Option<&HashSet<String>>,
    later_statements: &[CompiledParamPrefixStatement],
) -> bool {
    let mut bound_identifiers = Vec::new();
    collect_compiled_binding_pattern_identifiers(pattern, &mut bound_identifiers);
    bound_identifiers.into_iter().any(|ident| {
        body_identifier_references.is_none_or(|references| references.contains(&ident))
            || later_statements.iter().any(|statement| {
                compiled_prefix_statement_references_identifier(
                    allocator,
                    source_type,
                    statement,
                    &ident,
                )
            })
    })
}

fn collect_compiled_binding_pattern_identifiers(
    pattern: &CompiledBindingPattern,
    identifiers: &mut Vec<String>,
) {
    match pattern {
        CompiledBindingPattern::Identifier(name) => identifiers.push(name.clone()),
        CompiledBindingPattern::Object(object) => {
            for property in &object.properties {
                collect_compiled_binding_pattern_identifiers(&property.value, identifiers);
            }
            if let Some(rest) = &object.rest {
                collect_compiled_binding_pattern_identifiers(rest, identifiers);
            }
        }
        CompiledBindingPattern::Array(array) => {
            for element in array.elements.iter().flatten() {
                collect_compiled_binding_pattern_identifiers(element, identifiers);
            }
            if let Some(rest) = &array.rest {
                collect_compiled_binding_pattern_identifiers(rest, identifiers);
            }
        }
        CompiledBindingPattern::Assignment { left, .. } => {
            collect_compiled_binding_pattern_identifiers(left, identifiers);
        }
    }
}

fn compiled_prefix_statement_references_identifier(
    allocator: &Allocator,
    source_type: SourceType,
    statement: &CompiledParamPrefixStatement,
    ident: &str,
) -> bool {
    match &statement.init {
        CompiledInitializer::Identifier(name) => name == ident,
        CompiledInitializer::UndefinedFallback {
            temp_name,
            default_expr,
        } => {
            temp_name == ident
                || parse_expression_source(allocator, source_type, default_expr)
                    .ok()
                    .is_none_or(|expr| expression_references_identifier(&expr, ident))
        }
    }
}

fn collect_identifier_references_in_statements(
    statements: &oxc_allocator::Vec<'_, ast::Statement<'_>>,
) -> HashSet<String> {
    let mut collector = StatementIdentifierReferenceCollector::default();
    for statement in statements {
        collector.visit_statement(statement);
    }
    collector.references
}

#[derive(Default)]
struct StatementIdentifierReferenceCollector {
    references: HashSet<String>,
}

impl<'a> Visit<'a> for StatementIdentifierReferenceCollector {
    fn visit_identifier_reference(&mut self, it: &ast::IdentifierReference<'a>) {
        self.references.insert(it.name.to_string());
    }
}

fn build_compiled_prefix_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    statement: &CompiledParamPrefixStatement,
) -> Option<ast::Statement<'a>> {
    let pattern =
        build_compiled_binding_pattern(builder, allocator, source_type, &statement.pattern)?;
    let init = build_compiled_prefix_initializer(builder, allocator, source_type, &statement.init)?;
    Some(ast::Statement::VariableDeclaration(
        builder.alloc_variable_declaration(
            SPAN,
            statement.kind,
            builder.vec1(builder.variable_declarator(
                SPAN,
                statement.kind,
                pattern,
                NONE,
                Some(init),
                false,
            )),
            false,
        ),
    ))
}

fn build_compiled_binding_pattern<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    pattern: &CompiledBindingPattern,
) -> Option<ast::BindingPattern<'a>> {
    match pattern {
        CompiledBindingPattern::Identifier(name) => {
            Some(builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)))
        }
        CompiledBindingPattern::Object(object) => Some(build_compiled_object_pattern(
            builder,
            allocator,
            source_type,
            object,
        )),
        CompiledBindingPattern::Array(array) => Some(build_compiled_array_pattern(
            builder,
            allocator,
            source_type,
            array,
        )),
        CompiledBindingPattern::Assignment { left, default_expr } => {
            Some(builder.binding_pattern_assignment_pattern(
                SPAN,
                build_compiled_binding_pattern(builder, allocator, source_type, left)?,
                parse_expression_source(allocator, source_type, default_expr).ok()?,
            ))
        }
    }
}

fn build_compiled_object_pattern<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    object: &CompiledObjectPattern,
) -> ast::BindingPattern<'a> {
    let mut properties = builder.vec();
    for property in &object.properties {
        let value =
            build_compiled_binding_pattern(builder, allocator, source_type, &property.value)
                .expect("compiled property pattern should build");
        let key = build_compiled_property_key(builder, allocator, source_type, &property.key)
            .expect("compiled property key should build");
        properties.push(builder.binding_property(
            SPAN,
            key,
            value,
            property.shorthand,
            property.computed,
        ));
    }
    let rest = object.rest.as_ref().map(|rest| {
        builder.alloc_binding_rest_element(
            SPAN,
            build_compiled_binding_pattern(builder, allocator, source_type, rest)
                .expect("compiled rest pattern should build"),
        )
    });
    builder.binding_pattern_object_pattern(SPAN, properties, rest)
}

fn build_compiled_array_pattern<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    array: &CompiledArrayPattern,
) -> ast::BindingPattern<'a> {
    let elements = builder.vec_from_iter(array.elements.iter().map(|element| {
        element.as_ref().map(|pattern| {
            build_compiled_binding_pattern(builder, allocator, source_type, pattern)
                .expect("compiled array element pattern should build")
        })
    }));
    let rest = array.rest.as_ref().map(|rest| {
        builder.alloc_binding_rest_element(
            SPAN,
            build_compiled_binding_pattern(builder, allocator, source_type, rest)
                .expect("compiled array rest pattern should build"),
        )
    });
    builder.binding_pattern_array_pattern(SPAN, elements, rest)
}

fn build_compiled_property_key<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    key: &CompiledPropertyKey,
) -> Option<ast::PropertyKey<'a>> {
    match key {
        CompiledPropertyKey::StaticIdentifier(name) => {
            Some(builder.property_key_static_identifier(SPAN, builder.ident(name)))
        }
        CompiledPropertyKey::StringLiteral(value) => Some(ast::PropertyKey::from(
            builder.expression_string_literal(SPAN, builder.atom(value), None),
        )),
        CompiledPropertyKey::Source(source) => Some(ast::PropertyKey::from(
            parse_expression_source(allocator, source_type, source).ok()?,
        )),
    }
}

fn build_compiled_prefix_initializer<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    init: &CompiledInitializer,
) -> Option<ast::Expression<'a>> {
    match init {
        CompiledInitializer::Identifier(name) => {
            Some(builder.expression_identifier(SPAN, builder.ident(name)))
        }
        CompiledInitializer::UndefinedFallback {
            temp_name,
            default_expr,
        } => {
            let default_expr =
                parse_expression_source(allocator, source_type, default_expr).ok()?;
            let temp_ident = || builder.expression_identifier(SPAN, builder.ident(temp_name));
            Some(builder.expression_conditional(
                SPAN,
                builder.expression_binary(
                    SPAN,
                    temp_ident(),
                    BinaryOperator::StrictEquality,
                    builder.expression_identifier(SPAN, builder.ident("undefined")),
                ),
                default_expr,
                temp_ident(),
            ))
        }
    }
}

fn collect_preserved_original_body_statements<'a>(
    allocator: &'a Allocator,
    original_body: Option<&ast::FunctionBody<'_>>,
) -> oxc_allocator::Vec<'a, ast::Statement<'a>> {
    let mut statements = oxc_allocator::Vec::new_in(allocator);
    let Some(original_body) = original_body else {
        return statements;
    };
    for statement in &original_body.statements {
        if !crate::pipeline::should_preserve_leading_body_statement(statement) {
            break;
        }
        statements.push(statement.clone_in(allocator));
    }
    statements
}

fn find_original_compiled_function_body<'a>(
    stmt: &'a ast::Statement<'a>,
    cf: &CompiledFunction,
) -> Option<&'a ast::FunctionBody<'a>> {
    let mut finder = OriginalCompiledFunctionBodyFinder {
        start: cf.start,
        end: cf.end,
        body: None,
    };
    finder.visit_statement(stmt);
    finder.body.map(|body| unsafe { &*body })
}

struct OriginalCompiledFunctionBodyFinder<'a> {
    start: u32,
    end: u32,
    body: Option<*const ast::FunctionBody<'a>>,
}

impl<'a> Visit<'a> for OriginalCompiledFunctionBodyFinder<'a> {
    fn visit_function(&mut self, it: &ast::Function<'a>, flags: oxc_syntax::scope::ScopeFlags) {
        if self.body.is_none() && it.span.start == self.start && it.span.end == self.end {
            self.body = it.body.as_ref().map(|body| &**body as *const _);
            return;
        }
        walk::walk_function(self, it, flags);
    }

    fn visit_arrow_function_expression(&mut self, it: &ast::ArrowFunctionExpression<'a>) {
        if self.body.is_none() && it.span.start == self.start && it.span.end == self.end {
            self.body = Some(&*it.body as *const _);
            return;
        }
        walk::walk_arrow_function_expression(self, it);
    }
}

fn find_cache_initializer_index<'a>(
    statements: &oxc_allocator::Vec<'a, ast::Statement<'a>>,
    cache_import_name: &str,
) -> Option<usize> {
    statements
        .iter()
        .position(|statement| is_cache_initializer_statement(statement, cache_import_name))
}

fn is_cache_initializer_statement(statement: &ast::Statement<'_>, cache_import_name: &str) -> bool {
    let ast::Statement::VariableDeclaration(declaration) = statement else {
        return false;
    };
    declaration.declarations.iter().any(|declarator| {
        let Some(ast::Expression::CallExpression(call)) = declarator.init.as_ref() else {
            return false;
        };
        matches!(
            &call.callee,
            ast::Expression::Identifier(identifier) if identifier.name == cache_import_name
        )
    })
}

fn can_emit_compiled_statement_ast(cf: &CompiledFunction) -> bool {
    cf.body_payload == CompiledBodyPayload::LowerFromFinalHir
        && cf.is_function_declaration
        && !cf.needs_cache_import
}

fn maybe_gate_entrypoint_source(
    source_type: SourceType,
    source: String,
    gate_name: &str,
) -> String {
    let allocator = Allocator::default();
    let Ok(mut statements) = parse_statements(&allocator, source_type, &source) else {
        return source;
    };
    if statements.len() != 1 {
        return source;
    }

    let builder = AstBuilder::new(&allocator);
    let mut gater = FixtureEntrypointArrowGater {
        builder,
        gate_name,
        changed: false,
    };
    gater.visit_statement(&mut statements[0]);
    if !gater.changed {
        return source;
    }

    codegen_statement_source(&allocator, source_type, &statements[0])
}

struct FixtureEntrypointArrowGater<'a, 'gate> {
    builder: AstBuilder<'a>,
    gate_name: &'gate str,
    changed: bool,
}

impl<'a> VisitMut<'a> for FixtureEntrypointArrowGater<'a, '_> {
    fn visit_object_property(&mut self, property: &mut ast::ObjectProperty<'a>) {
        walk_mut::walk_object_property(self, property);

        let Some(key_name) = fixture_entrypoint_property_name(property) else {
            return;
        };
        if !matches!(key_name, "fn" | "useHook") || !is_empty_arrow_expression(&property.value) {
            return;
        }

        property.value = build_gated_empty_arrow_expression(self.builder, self.gate_name);
        self.changed = true;
    }
}

fn fixture_entrypoint_property_name<'a>(property: &ast::ObjectProperty<'a>) -> Option<&'a str> {
    match &property.key {
        ast::PropertyKey::StaticIdentifier(identifier) => Some(identifier.name.as_str()),
        ast::PropertyKey::StringLiteral(string) => Some(string.value.as_str()),
        _ => None,
    }
}

fn is_empty_arrow_expression(expression: &ast::Expression<'_>) -> bool {
    let ast::Expression::ArrowFunctionExpression(arrow) = expression.without_parentheses() else {
        return false;
    };
    !arrow.r#async
        && !arrow.expression
        && arrow.params.items.is_empty()
        && arrow.params.rest.is_none()
        && arrow.body.statements.is_empty()
}

fn build_gated_empty_arrow_expression<'a>(
    builder: AstBuilder<'a>,
    gate_name: &str,
) -> ast::Expression<'a> {
    builder.expression_conditional(
        SPAN,
        builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(gate_name)),
            NONE,
            builder.vec(),
            false,
        ),
        build_empty_arrow_expression(builder),
        build_empty_arrow_expression(builder),
    )
}

fn build_empty_arrow_expression<'a>(builder: AstBuilder<'a>) -> ast::Expression<'a> {
    builder.expression_arrow_function(
        SPAN,
        false,
        false,
        NONE,
        builder.alloc(builder.formal_parameters(
            SPAN,
            ast::FormalParameterKind::ArrowFormalParameters,
            builder.vec(),
            NONE,
        )),
        NONE,
        builder.alloc(builder.function_body(SPAN, builder.vec(), builder.vec())),
    )
}

fn build_inserted_import_statement<'a>(
    builder: AstBuilder<'a>,
    import_plan: &InsertedImport,
) -> ast::Statement<'a> {
    if import_plan.is_script {
        let mut properties = builder.vec();
        for spec in &import_plan.specs {
            let pattern =
                builder.binding_pattern_binding_identifier(SPAN, builder.ident(&spec.local));
            let key = if is_identifier_name(&spec.imported) {
                builder.property_key_static_identifier(SPAN, builder.ident(&spec.imported))
            } else {
                ast::PropertyKey::from(builder.expression_string_literal(
                    SPAN,
                    builder.atom(&spec.imported),
                    None,
                ))
            };
            properties.push(builder.binding_property(
                SPAN,
                key,
                pattern,
                spec.imported == spec.local && is_identifier_name(&spec.imported),
                false,
            ));
        }
        let object_pattern = builder.binding_pattern_object_pattern(SPAN, properties, NONE);
        let require_call = builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident("require")),
            NONE,
            builder.vec1(ast::Argument::from(builder.expression_string_literal(
                SPAN,
                builder.atom(&import_plan.source),
                None,
            ))),
            false,
        );
        ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
            SPAN,
            ast::VariableDeclarationKind::Const,
            builder.vec1(builder.variable_declarator(
                SPAN,
                ast::VariableDeclarationKind::Const,
                object_pattern,
                NONE,
                Some(require_call),
                false,
            )),
            false,
        ))
    } else {
        let specifiers = builder.vec_from_iter(import_plan.specs.iter().map(|spec| {
            let imported = if is_identifier_name(&spec.imported) {
                builder.module_export_name_identifier_name(SPAN, builder.atom(&spec.imported))
            } else {
                builder.module_export_name_string_literal(SPAN, builder.atom(&spec.imported), None)
            };
            builder.import_declaration_specifier_import_specifier(
                SPAN,
                imported,
                builder.binding_identifier(SPAN, builder.atom(&spec.local)),
                ast::ImportOrExportKind::Value,
            )
        }));
        ast::Statement::ImportDeclaration(builder.alloc_import_declaration(
            SPAN,
            Some(specifiers),
            builder.string_literal(SPAN, builder.atom(&import_plan.source), None),
            None,
            NONE,
            ast::ImportOrExportKind::Value,
        ))
    }
}

fn build_runtime_import_merge_statement<'a>(
    builder: AstBuilder<'a>,
    merged_specs: &[(String, String)],
) -> ast::Statement<'a> {
    build_inserted_import_statement(
        builder,
        &InsertedImport {
            source: "react/compiler-runtime".to_string(),
            specs: merged_specs
                .iter()
                .map(|(imported, local)| InsertedImportSpec {
                    imported: imported.clone(),
                    local: local.clone(),
                })
                .collect(),
            is_script: false,
        },
    )
}

fn parse_statements<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    code: &'a str,
) -> Result<oxc_allocator::Vec<'a, ast::Statement<'a>>, String> {
    let parsed = Parser::new(allocator, code, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse statement snippet: {} errors",
            parsed.errors.len()
        ));
    }
    Ok(parsed.program.body)
}

fn codegen_program(program: &ast::Program<'_>) -> String {
    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        ..CodegenOptions::default()
    };
    Codegen::new().with_options(options).build(program).code
}

fn codegen_statement_source(
    allocator: &Allocator,
    source_type: SourceType,
    statement: &ast::Statement<'_>,
) -> String {
    let builder = AstBuilder::new(allocator);
    let program = builder.program(
        SPAN,
        source_type,
        "",
        builder.vec(),
        None,
        builder.vec(),
        builder.vec1(statement.clone_in(allocator)),
    );
    codegen_program(&program)
}

fn codegen_expression_source(
    allocator: &Allocator,
    source_type: SourceType,
    expression: &ast::Expression<'_>,
) -> String {
    let builder = AstBuilder::new(allocator);
    let statement = builder.statement_expression(SPAN, expression.clone_in(allocator));
    codegen_statement_source(allocator, source_type, &statement)
        .trim()
        .trim_end_matches(';')
        .to_string()
}

fn is_simple_generated_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn collect_guard_alias_binding_map(
    bindings: &[crate::reactive_scopes::codegen_reactive::GeneratedBinding],
) -> HashMap<String, String> {
    bindings
        .iter()
        .filter(|binding| is_simple_generated_identifier(&binding.pattern))
        .map(|binding| (binding.expression.clone(), binding.pattern.clone()))
        .collect()
}

fn rewrite_guard_aliases_in_expression_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    expression: &mut ast::Expression<'a>,
    guard_aliases: &HashMap<String, String>,
) {
    let expression_source = codegen_expression_source(allocator, source_type, expression);
    if let Some(alias_name) = guard_aliases.get(&expression_source) {
        *expression = builder.expression_identifier(SPAN, builder.ident(alias_name));
        return;
    }
    if let ast::Expression::LogicalExpression(logical) = expression {
        rewrite_guard_aliases_in_expression_ast(
            builder,
            allocator,
            source_type,
            &mut logical.left,
            guard_aliases,
        );
        rewrite_guard_aliases_in_expression_ast(
            builder,
            allocator,
            source_type,
            &mut logical.right,
            guard_aliases,
        );
    }
}

fn rewrite_guard_aliases_in_statement_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    statement: &mut ast::Statement<'a>,
    guard_aliases: &HashMap<String, String>,
) {
    match statement {
        ast::Statement::IfStatement(if_statement) => {
            rewrite_guard_aliases_in_expression_ast(
                builder,
                allocator,
                source_type,
                &mut if_statement.test,
                guard_aliases,
            );
            rewrite_guard_aliases_in_statement_ast(
                builder,
                allocator,
                source_type,
                &mut if_statement.consequent,
                guard_aliases,
            );
            if let Some(alternate) = if_statement.alternate.as_mut() {
                rewrite_guard_aliases_in_statement_ast(
                    builder,
                    allocator,
                    source_type,
                    alternate,
                    guard_aliases,
                );
            }
        }
        ast::Statement::BlockStatement(block) => {
            for statement in block.body.iter_mut() {
                rewrite_guard_aliases_in_statement_ast(
                    builder,
                    allocator,
                    source_type,
                    statement,
                    guard_aliases,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn source_type_for_filename(filename: &str) -> SourceType {
    if filename.ends_with(".tsx") {
        SourceType::tsx()
    } else if filename.ends_with(".ts") {
        SourceType::ts().with_jsx(true)
    } else if filename.ends_with(".jsx") {
        SourceType::jsx()
    } else {
        SourceType::mjs().with_jsx(true)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use oxc_allocator::Allocator;
    use oxc_ast::{AstBuilder, ast};
    use oxc_span::SourceType;

    use crate::{
        codegen_backend::{CompiledOutlinedFunction, SynthesizedDefaultParamCache},
        environment::Environment,
        hir::types::{
            self, BasicBlock, BlockId, DeclarationId, Effect, HIR, HIRFunction, Identifier,
            IdentifierId, IdentifierName, MutableRange, Place, ReactFunctionType, SourceLocation,
            Terminal, Type,
        },
        options::EnvironmentConfig,
    };

    use super::{
        AstRenderState, CompiledBindingPattern, CompiledBodyPayload, CompiledFunction,
        CompiledInitializer, CompiledObjectPattern, CompiledParam, CompiledParamPrefixStatement,
        CompiledPropertyKey, codegen_statement_source, compute_transform_state,
        maybe_gate_entrypoint_source, normalize_compiled_body_for_hir_match,
        normalize_generated_body_flow_cast_marker_calls, parse_statements,
        restore_flow_cast_marker_calls, source_type_for_filename,
        try_rewrite_compiled_statement_ast,
    };
    use crate::codegen_backend::CompiledObjectPatternProperty;

    fn empty_test_state(source_type: oxc_span::SourceType) -> AstRenderState {
        AstRenderState {
            source_type,
            cache_import_name: "_c".to_string(),
            make_read_only_ident: String::new(),
            should_instrument_ident: String::new(),
            use_render_counter_ident: String::new(),
            hook_guard_ident: String::new(),
            structural_check_ident: String::new(),
            lower_context_access_ident: String::new(),
            lower_context_access_imported: String::new(),
            gating_local_name: None,
            imports_to_insert: vec![],
            runtime_import_merge_plan: None,
            instrument_source_path: String::new(),
        }
    }

    #[test]
    fn rewrites_flow_cast_bodies_to_marker_calls() {
        let body = r#"const x = { bar: props.bar };
const y = (x: Foo);
const z = ([]: Array<number>);
return z;"#;
        let normalized = normalize_generated_body_flow_cast_marker_calls(body);
        assert!(normalized.contains("__REACT_COMPILER_FLOW_CAST__<Foo>(x)"));
        assert!(normalized.contains("__REACT_COMPILER_FLOW_CAST__<Array<number>>([])"));
    }

    #[test]
    fn restores_flow_cast_marker_calls_to_flow_syntax() {
        let source = r#"const y = __REACT_COMPILER_FLOW_CAST__<Foo>(x);
const z = __REACT_COMPILER_FLOW_CAST__<Array<number>>([]);"#;
        let restored = restore_flow_cast_marker_calls(source);
        assert!(restored.contains("const y = (x: Foo);"));
        assert!(restored.contains("const z = ([]: Array<number>);"));
    }

    #[test]
    fn normalize_compiled_body_for_hir_match_canonicalizes_multiline_object_literals() {
        let lowered = r#"const country = Codes[code];
return {
  name: country.name,
  code
};"#;
        let rendered = r#"const country = Codes[code];
return { name: country.name, code };"#;

        assert_eq!(
            normalize_compiled_body_for_hir_match(lowered),
            normalize_compiled_body_for_hir_match(rendered)
        );
    }

    #[test]
    fn normalize_compiled_body_for_hir_match_canonicalizes_destructure_shorthand_pairs() {
        let lowered = r#"const { id, render } = t0;
return <Stringify key={id} render={render} />;"#;
        let rendered = r#"const { id: id, render: render } = t0;
return <Stringify key={id} render={render} />;"#;

        assert_eq!(
            normalize_compiled_body_for_hir_match(lowered),
            normalize_compiled_body_for_hir_match(rendered)
        );
    }

    #[test]
    fn normalize_compiled_body_for_hir_match_canonicalizes_destructure_brace_spacing() {
        let lowered = r#"const { id, name } = t0;
return <Stringify key={id} name={name} />;"#;
        let rendered = r#"const {id, name} = t0;
return <Stringify key={id} name={name} />;"#;

        assert_eq!(
            normalize_compiled_body_for_hir_match(lowered),
            normalize_compiled_body_for_hir_match(rendered)
        );
    }

    #[test]
    fn transform_state_ignores_nonsemantic_top_level_comments() {
        let source = r#"// @enableFire
import { fire } from "react";
/**
 * Fixture note
 */
function Component(props) {
  return null;
}"#;
        let output = r#"import { fire } from "react";
function Component(props) {
  return null;
}"#;
        assert!(!compute_transform_state(
            SourceType::mjs().with_jsx(true),
            output,
            source,
        ));
    }

    #[test]
    fn transform_state_keeps_nested_comment_deltas_semantic() {
        let source = r#"function Component(props) {
  useHook();
  // keep this inside the function body
  mutate(props.value);
}"#;
        let output = r#"function Component(props) {
  useHook();
  mutate(props.value);
}"#;
        assert!(compute_transform_state(
            SourceType::mjs().with_jsx(true),
            output,
            source,
        ));
    }

    #[test]
    fn transform_state_ignores_redundant_conditional_parentheses() {
        let source = r#"function ternary(props) {
  const a = props.a && props.b ? props.c || props.d : (props.e ?? props.f);
  const b = props.a ? (props.b && props.c ? props.d : props.e) : props.f;
  return a ? b : null;
}"#;
        let output = r#"function ternary(props) {
  const a = props.a && props.b ? props.c || props.d : props.e ?? props.f;
  const b = props.a ? props.b && props.c ? props.d : props.e : props.f;
  return a ? b : null;
}"#;
        assert!(!compute_transform_state(
            SourceType::mjs().with_jsx(true),
            output,
            source,
        ));
    }

    #[test]
    fn transform_state_keeps_cache_instrumentation_semantic() {
        let source = r#"function Component(props) {
  const value = props.value;
  return value;
}"#;
        let output = r#"import { c as _c } from "react/compiler-runtime";
function Component(props) {
  const $ = _c(1);
  let value;
  if ($[0] !== props.value) {
    value = props.value;
    $[0] = props.value;
  } else {
    value = $[0];
  }
  return value;
}"#;
        assert!(compute_transform_state(
            SourceType::mjs().with_jsx(true),
            output,
            source,
        ));
    }

    #[test]
    fn transform_state_ignores_conditional_set_state_bailout_fixture_delta() {
        let source = r#"function Component(props) {
  const [x, setX] = useState(0);

  const foo = () => {
    setX(1);
  };

  if (props.cond) {
    setX(2);
    foo();
  }

  return x;
}

export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: ['TodoAdd'],
  isComponent: 'TodoAdd',
};"#;
        let output = r#"function Component(props) {
  const [x, setX] = useState(0);
  const foo = (() => {
    setX(1);
  });
  if (props.cond) {
    setX(2);
    foo();
  }
  return x;
}
export const FIXTURE_ENTRYPOINT = {
  fn: Component,
  params: ["TodoAdd"],
  isComponent: "TodoAdd"
};"#;
        assert!(!compute_transform_state(
            SourceType::mjs().with_jsx(true),
            output,
            source,
        ));
    }

    #[test]
    fn rewrites_instrument_forget_prefix_as_ast_statement() {
        let source = r#"function Component(x) {
  return x;
}"#;
        let mut compiled_function = make_test_compiled_function(
            "Component",
            0,
            source.len() as u32,
            "return x;",
            &["x"],
            false,
        );
        compiled_function.needs_instrument_forget = true;
        let state = AstRenderState {
            should_instrument_ident: "shouldInstrument".to_string(),
            use_render_counter_ident: "useRenderCounter".to_string(),
            instrument_source_path: "fixture.jsx".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );

        assert!(rewritten.contains("if (DEV && shouldInstrument)"));
        assert!(rewritten.contains("useRenderCounter(\"Component\", \"fixture.jsx\")"));
    }

    #[test]
    fn rewrites_generated_prefix_statements_as_ast() {
        let source = r#"const FancyButton = (props) => { enum Color { Red } return null; };"#;
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.ts"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::ArrowFunctionExpression(arrow) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected arrow function");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "return value;",
            &["props"],
            true,
        );
        compiled_function.param_prefix_statements = vec![CompiledParamPrefixStatement {
            kind: ast::VariableDeclarationKind::Const,
            pattern: CompiledBindingPattern::Object(CompiledObjectPattern {
                properties: vec![CompiledObjectPatternProperty {
                    key: CompiledPropertyKey::StaticIdentifier("value".to_string()),
                    value: CompiledBindingPattern::Identifier("value".to_string()),
                    shorthand: true,
                    computed: false,
                }],
                rest: None,
            }),
            init: CompiledInitializer::Identifier("props".to_string()),
        }];

        let rewritten = rewrite_single_statement_for_test("fixture.ts", source, &compiled_function);

        assert!(rewritten.contains("const {"));
        assert!(rewritten.contains("value"));
        assert!(rewritten.contains("} = props;"));
        assert!(rewritten.contains("enum Color"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn prunes_generated_prefix_properties_using_ast_identifier_references() {
        let source = r#"const FancyButton = (props) => { return null; };"#;
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.ts"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::ArrowFunctionExpression(arrow) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected arrow function");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "console.log(\"value\");\nreturn other;",
            &["props"],
            true,
        );
        compiled_function.param_prefix_statements = vec![CompiledParamPrefixStatement {
            kind: ast::VariableDeclarationKind::Const,
            pattern: CompiledBindingPattern::Object(CompiledObjectPattern {
                properties: vec![
                    CompiledObjectPatternProperty {
                        key: CompiledPropertyKey::StaticIdentifier("value".to_string()),
                        value: CompiledBindingPattern::Identifier("value".to_string()),
                        shorthand: true,
                        computed: false,
                    },
                    CompiledObjectPatternProperty {
                        key: CompiledPropertyKey::StaticIdentifier("other".to_string()),
                        value: CompiledBindingPattern::Identifier("other".to_string()),
                        shorthand: true,
                        computed: false,
                    },
                ],
                rest: None,
            }),
            init: CompiledInitializer::Identifier("props".to_string()),
        }];

        let rewritten = rewrite_single_statement_for_test("fixture.ts", source, &compiled_function);

        assert!(rewritten.contains("const { other } = props;"));
        assert!(!rewritten.contains("value } = props;"));
        assert!(rewritten.contains("console.log(\"value\");"));
        assert!(rewritten.contains("return other;"));
    }

    #[test]
    fn skips_prefix_pruning_for_structural_undefined_fallbacks() {
        let source = r#"const FancyButton = (props) => { return null; };"#;
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.ts"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::ArrowFunctionExpression(arrow) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected arrow function");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "const value = t0 === undefined ? props.value : t0;\nreturn value;",
            &["props"],
            true,
        );
        compiled_function.param_prefix_statements = vec![CompiledParamPrefixStatement {
            kind: ast::VariableDeclarationKind::Const,
            pattern: CompiledBindingPattern::Object(CompiledObjectPattern {
                properties: vec![CompiledObjectPatternProperty {
                    key: CompiledPropertyKey::StaticIdentifier("value".to_string()),
                    value: CompiledBindingPattern::Identifier("value".to_string()),
                    shorthand: true,
                    computed: false,
                }],
                rest: None,
            }),
            init: CompiledInitializer::Identifier("props".to_string()),
        }];

        let rewritten = rewrite_single_statement_for_test("fixture.ts", source, &compiled_function);

        assert!(!rewritten.contains("const { value } = props;"));
        assert!(rewritten.contains("const value = t0 === undefined ? props.value : t0;"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn rewrites_preserved_directives_as_ast() {
        let source = r#"function Worklet(value) {
  return value;
}"#;
        let mut compiled_function = make_test_compiled_function(
            "Worklet",
            0,
            source.len() as u32,
            "return value;",
            &["value"],
            false,
        );
        compiled_function.directives = vec!["\"worklet\"".to_string()];

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("\"worklet\";"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn rewrites_runtime_helper_identifiers_as_ast() {
        let source = r#"function Component() {
  return null;
}"#;
        let mut compiled_function = make_test_compiled_function(
            "Component",
            0,
            source.len() as u32,
            "const cache = _c(1);\nreturn $dispatcherGuard(cache, lowerContextAccess($structuralCheck));",
            &[],
            false,
        );
        compiled_function.needs_hook_guards = true;
        compiled_function.needs_structural_check_import = true;
        compiled_function.needs_lower_context_access = true;
        let state = AstRenderState {
            cache_import_name: "_cache".to_string(),
            hook_guard_ident: "hookGuard".to_string(),
            structural_check_ident: "structuralCheck".to_string(),
            lower_context_access_ident: "loweredContext".to_string(),
            lower_context_access_imported: "lowerContextAccess".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );

        assert!(rewritten.contains("const cache = _cache(1);"));
        assert!(rewritten.contains("hookGuard(cache, loweredContext(structuralCheck));"));
    }

    #[test]
    fn rewrites_function_hook_guard_wrapper_as_ast() {
        let source = r#"function Component() {
  return null;
}"#;
        let mut compiled_function = make_test_compiled_function(
            "Component",
            0,
            source.len() as u32,
            "return null;",
            &[],
            false,
        );
        compiled_function.needs_hook_guards = true;
        compiled_function.needs_function_hook_guard_wrapper = true;
        let state = AstRenderState {
            hook_guard_ident: "hookGuard".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );

        assert!(rewritten.contains("try {"));
        assert!(rewritten.contains("hookGuard(0);"));
        assert!(rewritten.contains("return null;"));
        assert!(rewritten.contains("finally {"));
        assert!(rewritten.contains("hookGuard(1);"));
    }

    #[test]
    fn normalizes_use_fire_binding_temp_names_as_ast() {
        let source = r#"function Component() { return null; }"#;
        let mut compiled_function = make_test_compiled_function(
            "Component",
            0,
            source.len() as u32,
            "let t1 = useFire(foo);\nlet t0 = useFire(bar);\nreturn [t1, t0];",
            &[],
            false,
        );
        compiled_function.normalize_use_fire_binding_temps = true;

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("let t0 = useFire(foo);"));
        assert!(rewritten.contains("let t1 = useFire(bar);"));
        assert!(rewritten.contains("return [t0, t1];"));
    }

    #[test]
    fn rewrites_nested_arrow_body_from_hir_ast() {
        let source = "const FancyButton = (props) => null;";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::ArrowFunctionExpression(arrow) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected arrow function");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "return props;",
            &["props"],
            true,
        );
        compiled_function.body_payload = CompiledBodyPayload::LowerFromFinalHir;
        compiled_function.hir_function = Some(simple_return_param_hir("props"));

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const FancyButton = (props) => {"));
        assert!(rewritten.contains("return props;"));
    }

    #[test]
    fn rewrites_emit_freeze_cache_store_as_ast() {
        let source = r#"function useFoo(props) {
  return null;
}"#;
        let mut compiled_function = make_test_compiled_function(
            "useFoo",
            0,
            source.len() as u32,
            "let t0;\nif ($[0] !== props.value) {\n  t0 = props.value;\n  $[0] = props.value;\n  $[1] = t0;\n} else {\n  t0 = $[1];\n}\nreturn t0;",
            &["props"],
            false,
        );
        compiled_function.needs_emit_freeze = true;
        let state = AstRenderState {
            make_read_only_ident: "makeReadOnly".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );

        assert!(rewritten.contains("$[1] = __DEV__ ? makeReadOnly(t0, \"useFoo\") : t0;"));
    }

    fn make_test_compiled_function(
        name: &str,
        start: u32,
        end: u32,
        generated_body: &str,
        params: &[&str],
        _is_arrow: bool,
    ) -> CompiledFunction {
        CompiledFunction {
            name: name.to_string(),
            start,
            end,
            generated_body: Some(generated_body.to_string()),
            generated_body_shape:
                crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Unknown,
            body_payload: CompiledBodyPayload::GeneratedString,
            needs_cache_import: false,
            compiled_params: Some(
                params
                    .iter()
                    .map(|name| CompiledParam {
                        name: (*name).to_string(),
                        is_rest: false,
                    })
                    .collect(),
            ),
            param_prefix_statements: vec![],
            synthesized_default_param_cache: None,
            is_async: false,
            is_generator: false,
            is_function_declaration: false,
            directives: vec![],
            hir_function: None,
            cache_prologue: None,
            needs_function_hook_guard_wrapper: false,
            normalize_use_fire_binding_temps: false,
            needs_instrument_forget: false,
            needs_emit_freeze: false,
            outlined_functions: vec![],
            hir_outlined_functions: vec![],
            has_fire_rewrite: false,
            needs_hook_guards: false,
            needs_structural_check_import: false,
            needs_lower_context_access: false,
        }
    }

    fn simple_return_param_hir(name: &str) -> HIRFunction {
        let param = named_place(0, 0, name);
        HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![types::Argument::Place(param.clone())],
            returns: param.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![(
                    BlockId::new(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId::new(0),
                        instructions: vec![],
                        terminal: Terminal::Return {
                            value: param.clone(),
                            return_variant: types::ReturnVariant::Explicit,
                            id: types::InstructionId::new(0),
                            loc: SourceLocation::Generated,
                        },
                        preds: HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn named_place(id: u32, declaration_id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId::new(id),
                declaration_id: DeclarationId::new(declaration_id),
                name: Some(IdentifierName::Named(name.to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn rewrite_single_statement_for_test(
        filename: &str,
        source: &str,
        compiled_function: &CompiledFunction,
    ) -> String {
        rewrite_single_statement_for_test_with_state(
            filename,
            source,
            compiled_function,
            empty_test_state(source_type_for_filename(filename)),
        )
    }

    fn rewrite_single_statement_for_test_with_state(
        filename: &str,
        source: &str,
        compiled_function: &CompiledFunction,
        state: AstRenderState,
    ) -> String {
        let allocator = Allocator::default();
        let source_type = source_type_for_filename(filename);
        let mut statements = parse_statements(&allocator, source_type, source).unwrap();
        let statement = statements.pop().unwrap();
        let builder = AstBuilder::new(&allocator);
        let rewritten = try_rewrite_compiled_statement_ast(
            builder,
            &allocator,
            source_type,
            source,
            &statement,
            &[compiled_function],
            &state,
        )
        .expect("expected AST-native rewrite");
        rewritten
            .into_iter()
            .map(|statement| {
                codegen_statement_source(&allocator, source_type, &statement)
                    .trim_end_matches('\n')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rewrite_single_statement_for_test_with_many(
        filename: &str,
        source: &str,
        compiled_functions: &[&CompiledFunction],
        state: AstRenderState,
    ) -> String {
        let allocator = Allocator::default();
        let source_type = source_type_for_filename(filename);
        let mut statements = parse_statements(&allocator, source_type, source).unwrap();
        let statement = statements.pop().unwrap();
        let builder = AstBuilder::new(&allocator);
        let rewritten = try_rewrite_compiled_statement_ast(
            builder,
            &allocator,
            source_type,
            source,
            &statement,
            compiled_functions,
            &state,
        )
        .expect("expected AST-native rewrite");
        rewritten
            .into_iter()
            .map(|statement| {
                codegen_statement_source(&allocator, source_type, &statement)
                    .trim_end_matches('\n')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parses_jsx_statement_snippet() {
        let allocator = Allocator::default();
        let statements = parse_statements(
            &allocator,
            source_type_for_filename("fixture.jsx"),
            "function Component() { return <div />; }",
        )
        .unwrap();
        assert_eq!(statements.len(), 1);
    }

    #[test]
    fn gates_empty_fixture_entrypoint_arrows() {
        let input =
            "export let FIXTURE_ENTRYPOINT = { fn: () =>{}, useHook: () =>{} };".to_string();
        let output =
            maybe_gate_entrypoint_source(source_type_for_filename("fixture.jsx"), input, "gate");
        assert!(output.contains("fn: gate() ?"));
        assert!(output.contains("useHook: gate() ?"));
        assert_eq!(output.matches("() =>").count(), 4);
    }

    #[test]
    fn rewrites_memo_wrapped_function_expression_as_ast() {
        let source =
            "const FancyButton = React.memo(function FancyButton(props) { return null; });";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::CallExpression(call) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected call initializer");
        };
        let ast::Argument::FunctionExpression(function) = &call.arguments[0] else {
            panic!("expected function expression argument");
        };

        let compiled_function = make_test_compiled_function(
            "FancyButton",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props"],
            false,
        );
        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);
        assert!(rewritten.contains("React.memo(function FancyButton(props) {"));
        assert!(rewritten.contains("return <div />;"));
    }

    #[test]
    fn rewrites_assignment_arrow_expression_as_ast() {
        let source = "FancyButton = () => null;";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::ExpressionStatement(expression_statement) = statement else {
            panic!("expected expression statement");
        };
        let ast::Expression::AssignmentExpression(assignment) = &expression_statement.expression
        else {
            panic!("expected assignment expression");
        };
        let arrow = match &assignment.right {
            ast::Expression::ArrowFunctionExpression(arrow) => arrow,
            _ => panic!("expected arrow function"),
        };

        let compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "return <div />;",
            &[],
            true,
        );
        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);
        assert!(rewritten.contains("FancyButton = () => {"));
        assert!(rewritten.contains("return <div />;"));
    }

    #[test]
    fn rewrites_export_default_forward_ref_call_as_ast() {
        let source =
            "export default React.forwardRef(function FancyButton(props, ref) { return null; });";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::ExportDefaultDeclaration(export_default) = statement else {
            panic!("expected export default declaration");
        };
        let ast::ExportDefaultDeclarationKind::CallExpression(call) = &export_default.declaration
        else {
            panic!("expected call export default");
        };
        let ast::Argument::FunctionExpression(function) = &call.arguments[0] else {
            panic!("expected function expression argument");
        };

        let compiled_function = make_test_compiled_function(
            "FancyButton",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props", "ref"],
            false,
        );
        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);
        assert!(
            rewritten
                .contains("export default React.forwardRef(function FancyButton(props, ref) {")
        );
        assert!(rewritten.contains("return <div />;"));
    }

    #[test]
    fn rewrites_ts_as_wrapped_arrow_as_ast() {
        let source = "const FancyButton = ((props) => null) as any;";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.tsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::TSAsExpression(ts_as_expression) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected ts as expression");
        };
        let ast::Expression::ParenthesizedExpression(parenthesized) = &ts_as_expression.expression
        else {
            panic!("expected parenthesized arrow");
        };
        let ast::Expression::ArrowFunctionExpression(arrow) = &parenthesized.expression else {
            panic!("expected arrow function");
        };

        let compiled_function = make_test_compiled_function(
            "FancyButton",
            arrow.span.start,
            arrow.span.end,
            "return <div />;",
            &["props"],
            true,
        );
        let rewritten =
            rewrite_single_statement_for_test("fixture.tsx", source, &compiled_function);
        assert!(rewritten.contains("const FancyButton = ((props) => {"));
        assert!(rewritten.contains("return <div />;"));
        assert!(rewritten.contains("}) as any;"));
    }

    #[test]
    fn rewrites_gated_memo_wrapped_function_expression_as_ast() {
        let source =
            "const FancyButton = React.memo(function FancyButton(props) { return null; });";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::CallExpression(call) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected initializer")
        else {
            panic!("expected call initializer");
        };
        let ast::Argument::FunctionExpression(function) = &call.arguments[0] else {
            panic!("expected function expression argument");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props"],
            false,
        );
        compiled_function.needs_cache_import = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );
        assert!(rewritten.contains("React.memo(gate() ? function FancyButton(props) {"));
        assert!(rewritten.contains(": function FancyButton(props) {"));
    }

    #[test]
    fn rewrites_gated_export_default_forward_ref_as_ast() {
        let source =
            "export default React.forwardRef(function FancyButton(props, ref) { return null; });";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::ExportDefaultDeclaration(export_default) = statement else {
            panic!("expected export default declaration");
        };
        let ast::ExportDefaultDeclarationKind::CallExpression(call) = &export_default.declaration
        else {
            panic!("expected call export default");
        };
        let ast::Argument::FunctionExpression(function) = &call.arguments[0] else {
            panic!("expected function expression argument");
        };

        let mut compiled_function = make_test_compiled_function(
            "FancyButton",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props", "ref"],
            false,
        );
        compiled_function.needs_cache_import = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );
        assert!(rewritten.contains(
            "export default React.forwardRef(gate() ? function FancyButton(props, ref) {"
        ));
        assert!(rewritten.contains(": function FancyButton(props, ref) {"));
    }

    #[test]
    fn rewrites_multiple_gated_wrapped_functions_in_one_statement_as_ast() {
        let source =
            "const First = React.memo((first) => null), Second = React.memo((second) => null);";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::VariableDeclaration(variable) = statement else {
            panic!("expected variable declaration");
        };
        let ast::Expression::CallExpression(first_call) = variable.declarations[0]
            .init
            .as_ref()
            .expect("expected first initializer")
        else {
            panic!("expected first call initializer");
        };
        let ast::Expression::CallExpression(second_call) = variable.declarations[1]
            .init
            .as_ref()
            .expect("expected second initializer")
        else {
            panic!("expected second call initializer");
        };
        let ast::Argument::ArrowFunctionExpression(first_arrow) = &first_call.arguments[0] else {
            panic!("expected first arrow");
        };
        let ast::Argument::ArrowFunctionExpression(second_arrow) = &second_call.arguments[0] else {
            panic!("expected second arrow");
        };

        let mut first = make_test_compiled_function(
            "First",
            first_arrow.span.start,
            first_arrow.span.end,
            "return <div />;",
            &["first"],
            true,
        );
        first.needs_cache_import = true;
        let mut second = make_test_compiled_function(
            "Second",
            second_arrow.span.start,
            second_arrow.span.end,
            "return <span />;",
            &["second"],
            true,
        );
        second.needs_cache_import = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_many(
            "fixture.jsx",
            source,
            &[&first, &second],
            state,
        );
        assert!(rewritten.contains("React.memo(gate() ? (first) => {"));
        assert!(rewritten.contains(": (first) => null)"));
        assert!(rewritten.contains("React.memo(gate() ? (second) => {"));
        assert!(rewritten.contains(": (second) => null)"));
    }

    #[test]
    fn rewrites_gated_export_named_function_declaration_as_ast() {
        let source = "export function Bar(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::ExportNamedDeclaration(export_named) = statement else {
            panic!("expected export named declaration");
        };
        let ast::Declaration::FunctionDeclaration(function) = export_named
            .declaration
            .as_ref()
            .expect("expected declaration")
        else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Bar",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props"],
            false,
        );
        compiled_function.needs_cache_import = true;
        compiled_function.is_function_declaration = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );
        assert!(rewritten.contains("export const Bar = gate() ? function Bar(props) {"));
        assert!(rewritten.contains(": function Bar(props) {"));
    }

    #[test]
    fn rewrites_gated_export_default_function_declaration_as_ast() {
        let source = "export default function Bar(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::ExportDefaultDeclaration(export_default) = statement else {
            panic!("expected export default declaration");
        };
        let ast::ExportDefaultDeclarationKind::FunctionDeclaration(function) =
            &export_default.declaration
        else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Bar",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props"],
            false,
        );
        compiled_function.needs_cache_import = true;
        compiled_function.is_function_declaration = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );
        assert!(rewritten.contains("const Bar = gate() ? function Bar(props) {"));
        assert!(rewritten.contains("export default Bar;"));
    }

    #[test]
    fn rewrites_gated_function_declaration_used_before_decl_as_ast() {
        let source = "export default memo(Foo);\nfunction Foo({prop1, prop2}) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["t0"],
            false,
        );
        compiled_function.needs_cache_import = true;
        compiled_function.is_function_declaration = true;
        let mut state = empty_test_state(source_type_for_filename("fixture.jsx"));
        state.gating_local_name = Some("gate".to_string());

        let rewritten = rewrite_single_statement_for_test_with_state(
            "fixture.jsx",
            source,
            &compiled_function,
            state,
        );
        assert!(rewritten.contains("const gate_result = gate();"));
        assert!(rewritten.contains("function Foo_optimized(t0) {"));
        assert!(rewritten.contains("function Foo_unoptimized({ prop1, prop2 }) {"));
        assert!(rewritten.contains("function Foo(arg0) {"));
        assert!(rewritten.contains("if (gate_result)"));
    }

    #[test]
    fn appends_rendered_async_outlined_function_as_ast() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return <div />;",
            &["props"],
            false,
        );
        compiled_function.is_function_declaration = true;
        compiled_function.outlined_functions = vec![CompiledOutlinedFunction {
            name: "_temp".to_string(),
            params: vec![
                CompiledParam {
                    name: "load".to_string(),
                    is_rest: false,
                },
                CompiledParam {
                    name: "rest".to_string(),
                    is_rest: true,
                },
            ],
            body: Some("return await load(...rest);".to_string()),
            body_shape: crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Unknown,
            directives: vec![],
            cache_prologue: None,
            needs_function_hook_guard_wrapper: false,
            is_async: true,
            is_generator: false,
        }];

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("function Foo(props) {"));
        assert!(rewritten.contains("async function _temp(load, ...rest) {"));
        assert!(rewritten.contains("return await load(...rest);"));
    }

    #[test]
    fn prepends_synthesized_default_param_cache_as_ast() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return value;",
            &["t0"],
            false,
        );
        compiled_function.is_function_declaration = true;
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 2,
                fast_refresh: None,
            });
        compiled_function.synthesized_default_param_cache = Some(SynthesizedDefaultParamCache {
            value_name: "value".to_string(),
            temp_name: "t0".to_string(),
            value_expr: "props.value".to_string(),
        });
        compiled_function.generated_body = None;

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const $ = _c(2);"));
        assert!(rewritten.contains("let value;"));
        assert!(rewritten.contains("if ($[0] !== t0) {"));
        assert!(rewritten.contains("value = t0 === undefined ? props.value : t0;"));
        assert!(rewritten.contains("$[1] = value;"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_return_identifier_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return value;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnIdentifier(
                "value".to_string(),
            );

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("function Foo(props) {"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_expression_statements_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "useEffect(t2, [arr]);",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ExpressionStatements(
                vec!["useEffect(t2, [arr])".to_string()],
            );

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("useEffect(t2, [arr]);"));
    }

    #[test]
    fn builds_return_expression_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return [props.left, props.right];",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnExpression(
                "[props.left, props.right]".to_string(),
            );

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("function Foo(props) {"));
        assert!(rewritten.contains("return [props.left, props.right];"));
    }

    #[test]
    fn builds_bound_expression_return_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "const value = [props.left, props.right]; return value;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::BoundExpressionReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Const,
                expression: "[props.left, props.right]".to_string(),
            };

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("function Foo(props) {"));
        assert!(rewritten.contains("const value = [props.left, props.right];"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_assigned_expression_return_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let value; value = [props.left, props.right]; return value;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AssignedExpressionReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                expression: "[props.left, props.right]".to_string(),
            };

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("let value;"));
        assert!(rewritten.contains("value = [props.left, props.right];"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_wrapped_return_expression_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let t0; if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = { click: _temp }; $[0] = t0; } else { t0 = $[0]; } return useRef(t0);",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::WrappedReturnExpression {
                source_name: "t0".to_string(),
                expression: "useRef(t0)".to_string(),
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
                        value_name: "t0".to_string(),
                        value_kind: ast::VariableDeclarationKind::Let,
                        value_slot: 0,
                        memoized_bindings: vec![],
                        memoized_assignments: vec![],
                        memoized_expressions: vec![],
                        memoized_setup_statements: vec![],
                        memoized_expr: Some("{ click: _temp }".to_string()),
                    },
                ),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {"));
        assert!(rewritten.contains("$[0] = t0;"));
        assert!(rewritten.contains("return useRef(t0);"));
    }

    #[test]
    fn builds_zero_dependency_existing_return_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = { click: _temp }; $[0] = t0; } else { t0 = $[0]; } return t0;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedExistingReturn {
                value_name: "t0".to_string(),
                value_slot: 0,
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![],
                memoized_expr: Some("{ click: _temp }".to_string()),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(!rewritten.contains("let t0;"));
        assert!(rewritten.contains("if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {"));
        assert!(rewritten.contains("t0 = { click: _temp };"));
        assert!(rewritten.contains("$[0] = t0;"));
        assert!(rewritten.contains("return t0;"));
    }

    #[test]
    fn builds_aliased_return_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let t0; if ($[0] !== props.items) { t0 = props.items.map(_temp); $[0] = props.items; $[1] = t0; } else { t0 = $[1]; } const mapped = t0; return mapped;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::AliasedReturn {
                alias_name: "mapped".to_string(),
                alias_kind: ast::VariableDeclarationKind::Const,
                source_name: "t0".to_string(),
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
                        value_name: "t0".to_string(),
                        value_kind: ast::VariableDeclarationKind::Let,
                        dep_slot: 0,
                        dep_expr: "props.items".to_string(),
                        value_slot: 1,
                        memoized_bindings: vec![],
                        memoized_assignments: vec![],
                        memoized_expressions: vec![],
                        memoized_setup_statements: vec![],
                        memoized_expr: Some("props.items.map(_temp)".to_string()),
                    },
                ),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 2,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const mapped = t0;"));
        assert!(rewritten.contains("return mapped;"));
    }

    #[test]
    fn builds_prefixed_binding_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "const f = _temp; let t0; if ($[0] !== props.items) { t0 = props.items.map(f); $[0] = props.items; $[1] = t0; } else { t0 = $[1]; } return t0;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
                bindings: vec![
                    crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                        kind: ast::VariableDeclarationKind::Const,
                        pattern: "f".to_string(),
                        expression: "_temp".to_string(),
                    },
                ],
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
                        value_name: "t0".to_string(),
                        value_kind: ast::VariableDeclarationKind::Let,
                        dep_slot: 0,
                        dep_expr: "props.items".to_string(),
                        value_slot: 1,
                        memoized_bindings: vec![],
                        memoized_assignments: vec![],
                        memoized_expressions: vec![],
                        memoized_setup_statements: vec![],
                        memoized_expr: Some("props.items.map(f)".to_string()),
                    },
                ),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 2,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const f = _temp;"));
        assert!(rewritten.contains("t0 = props.items.map(f);"));
    }

    #[test]
    fn preserves_guard_alias_bindings_in_prefixed_memo_body() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let c = $0[0] !== props.value; let results; if (c) { results = identity(props.value); $0[0] = props.value; $0[1] = results; } else { results = $0[1]; } return results;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
                bindings: vec![
                    crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                        kind: ast::VariableDeclarationKind::Let,
                        pattern: "c".to_string(),
                        expression: "$0[0] !== props.value".to_string(),
                    },
                ],
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedDeclarations {
                        declarations: vec![
                            crate::reactive_scopes::codegen_reactive::GeneratedDeclaration {
                                kind: ast::VariableDeclarationKind::Let,
                                pattern: "results".to_string(),
                            },
                        ],
                        inner: Box::new(
                            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedExistingReturn {
                                value_name: "results".to_string(),
                                dep_slot: 0,
                                dep_expr: "props.value".to_string(),
                                value_slot: 1,
                                memoized_bindings: vec![],
                                memoized_assignments: vec![],
                                memoized_expressions: vec![],
                                memoized_setup_statements: vec![],
                                memoized_expr: Some("identity(props.value)".to_string()),
                            },
                        ),
                    },
                ),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$0".to_string(),
                size: 2,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("let c = $0[0] !== props.value;"));
        assert!(rewritten.contains("if (c) {"));
        assert!(!rewritten.contains("if ($0[0] !== props.value) {"));
    }

    #[test]
    fn builds_sequential_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let t0; if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { t0 = props.items.map(_temp); $[0] = t0; } else { t0 = $[0]; } const mapped = t0; let t1; if ($[1] !== mapped) { t1 = mapped.slice(0); $[1] = mapped; $[2] = t1; } else { t1 = $[2]; } return t1;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::Sequential {
                prefix: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
                        value_name: "t0".to_string(),
                        value_kind: ast::VariableDeclarationKind::Let,
                        value_slot: 0,
                        memoized_bindings: vec![],
                        memoized_assignments: vec![],
                        memoized_expressions: vec![],
                        memoized_setup_statements: vec![],
                        memoized_expr: Some("props.items.map(_temp)".to_string()),
                    },
                ),
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
                        bindings: vec![
                            crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                                kind: ast::VariableDeclarationKind::Const,
                                pattern: "mapped".to_string(),
                                expression: "t0".to_string(),
                            },
                        ],
                        inner: Box::new(
                            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
                                value_name: "t1".to_string(),
                                value_kind: ast::VariableDeclarationKind::Let,
                                dep_slot: 1,
                                dep_expr: "mapped".to_string(),
                                value_slot: 2,
                                memoized_bindings: vec![],
                                memoized_assignments: vec![],
                                memoized_expressions: vec![],
                                memoized_setup_statements: vec![],
                                memoized_expr: Some("mapped.slice(0)".to_string()),
                            },
                        ),
                    },
                ),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 3,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {"));
        assert!(rewritten.contains("const mapped = t0;"));
        assert!(rewritten.contains("if ($[1] !== mapped) {"));
        assert!(rewritten.contains("$[2] = t1;"));
        assert!(rewritten.contains("return t1;"));
    }

    #[test]
    fn builds_prefixed_assignment_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "const mapped = props.items; foo = mapped; return mapped;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
                bindings: vec![crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                    kind: ast::VariableDeclarationKind::Const,
                    pattern: "mapped".to_string(),
                    expression: "props.items".to_string(),
                }],
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedAssignments {
                        assignments: vec![
                            crate::reactive_scopes::codegen_reactive::GeneratedAssignment {
                                target: "foo".to_string(),
                                value: "mapped".to_string(),
                            },
                        ],
                        inner: Box::new(
                            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnIdentifier(
                                "mapped".to_string(),
                            ),
                        ),
                    },
                ),
            };

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const mapped = props.items;"));
        assert!(rewritten.contains("foo = mapped;"));
        assert!(rewritten.contains("return mapped;"));
    }

    #[test]
    fn builds_destructured_prefixed_binding_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "const { id } = props; return id;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedBindings {
                bindings: vec![crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                    kind: ast::VariableDeclarationKind::Const,
                    pattern: "{ id }".to_string(),
                    expression: "props".to_string(),
                }],
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnIdentifier(
                        "id".to_string(),
                    ),
                ),
            };

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const { id } = props;"));
        assert!(rewritten.contains("return id;"));
    }

    #[test]
    fn builds_prefixed_expression_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "useRenamed(); return props.items;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::PrefixedExpressionStatements {
                expressions: vec!["useRenamed()".to_string()],
                inner: Box::new(
                    crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ReturnExpression(
                        "props.items".to_string(),
                    ),
                ),
            };

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("useRenamed();"));
        assert!(rewritten.contains("return props.items;"));
    }

    #[test]
    fn builds_zero_dependency_memoized_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let value; if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { value = [props.left, props.right]; $[0] = value; } else { value = $[0]; } return value;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                value_slot: 0,
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![],
                memoized_expr: Some("[props.left, props.right]".to_string()),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const $ = _c(1);"));
        assert!(rewritten.contains("if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {"));
        assert!(rewritten.contains("$[0] = value;"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_memoized_branch_assignments_from_shape_without_generated_source() {
        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                value_slot: 0,
                memoized_bindings: vec![
                    crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                        kind: ast::VariableDeclarationKind::Const,
                        pattern: "x".to_string(),
                        expression: "[{}]".to_string(),
                    },
                    crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                        kind: ast::VariableDeclarationKind::Const,
                        pattern: "y".to_string(),
                        expression: "x.map(_temp)".to_string(),
                    },
                ],
                memoized_assignments: vec![
                    crate::reactive_scopes::codegen_reactive::GeneratedAssignment {
                        target: "y[0].flag".to_string(),
                        value: "true".to_string(),
                    },
                ],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![],
                memoized_expr: Some("[x, y]".to_string()),
            };
        let function_body = super::try_build_function_body_from_shape(
            builder,
            &allocator,
            source_type_for_filename("fixture.jsx"),
            &body_shape,
            Some(&crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            }),
        )
        .expect("expected AST-native body");
        let rewritten = function_body
            .statements
            .iter()
            .map(|statement| {
                codegen_statement_source(
                    &allocator,
                    source_type_for_filename("fixture.jsx"),
                    statement,
                )
                .trim_end_matches('\n')
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rewritten.contains("const x = [{}];"));
        assert!(rewritten.contains("const y = x.map(_temp);"));
        assert!(rewritten.contains("y[0].flag = true;"));
        assert!(rewritten.contains("value = [x, y];"));
    }

    #[test]
    fn builds_memoized_branch_expression_statements_from_shape_without_generated_source() {
        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::ZeroDependencyMemoizedReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                value_slot: 0,
                memoized_bindings: vec![crate::reactive_scopes::codegen_reactive::GeneratedBinding {
                    kind: ast::VariableDeclarationKind::Const,
                    pattern: "x".to_string(),
                    expression: "[]".to_string(),
                }],
                memoized_assignments: vec![],
                memoized_expressions: vec!["x.push(a)".to_string()],
                memoized_setup_statements: vec![],
                memoized_expr: Some("[x, a]".to_string()),
            };
        let function_body = super::try_build_function_body_from_shape(
            builder,
            &allocator,
            source_type_for_filename("fixture.jsx"),
            &body_shape,
            Some(&crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            }),
        )
        .expect("expected AST-native body");
        let rewritten = function_body
            .statements
            .iter()
            .map(|statement| {
                codegen_statement_source(
                    &allocator,
                    source_type_for_filename("fixture.jsx"),
                    statement,
                )
                .trim_end_matches('\n')
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rewritten.contains("const x = [];"));
        assert!(rewritten.contains("x.push(a);"));
        assert!(rewritten.contains("value = [x, a];"));
    }

    #[test]
    fn builds_memoized_statement_setup_from_shape_without_generated_source() {
        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
                value_name: "items".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                dep_slot: 0,
                dep_expr: "props.a".to_string(),
                value_slot: 1,
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![
                    "items = [];".to_string(),
                    "items.push(props.a);".to_string(),
                ],
                memoized_expr: None,
            };
        let function_body = super::try_build_function_body_from_shape(
            builder,
            &allocator,
            source_type_for_filename("fixture.jsx"),
            &body_shape,
            Some(&crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 2,
                fast_refresh: None,
            }),
        )
        .expect("expected AST-native body");
        let rewritten = function_body
            .statements
            .iter()
            .map(|statement| {
                codegen_statement_source(
                    &allocator,
                    source_type_for_filename("fixture.jsx"),
                    statement,
                )
                .trim_end_matches('\n')
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rewritten.contains("items = [];"));
        assert!(rewritten.contains("items.push(props.a);"));
        assert!(rewritten.contains("$[0] = props.a;"));
        assert!(rewritten.contains("$[1] = items;"));
    }

    #[test]
    fn builds_single_dependency_memoized_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let items; if ($[0] !== props.items) { items = props.items.map(_temp); $[0] = props.items; $[1] = items; } else { items = $[1]; } return items;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleDependencyMemoizedReturn {
                value_name: "items".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                dep_slot: 0,
                dep_expr: "props.items".to_string(),
                value_slot: 1,
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![],
                memoized_expr: Some("props.items.map(_temp)".to_string()),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 2,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const $ = _c(2);"));
        assert!(rewritten.contains("let items;"));
        assert!(rewritten.contains("if ($[0] !== props.items) {"));
        assert!(rewritten.contains("items = props.items.map(_temp);"));
        assert!(rewritten.contains("$[1] = items;"));
        assert!(rewritten.contains("return items;"));
    }

    #[test]
    fn builds_multi_dependency_memoized_body_from_shape_without_generated_source() {
        let source = "function Foo(props) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let value; if ($[0] !== props.left || $[1] !== props.right) { value = [props.left, props.right]; $[0] = props.left; $[1] = props.right; $[2] = value; } else { value = $[2]; } return value;",
            &["props"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::MultiDependencyMemoizedReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                deps: vec![
                    (0, "props.left".to_string()),
                    (1, "props.right".to_string()),
                ],
                value_slot: 2,
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_setup_statements: vec![],
                memoized_expr: Some("[props.left, props.right]".to_string()),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 3,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const $ = _c(3);"));
        assert!(rewritten.contains("if ($[0] !== props.left || $[1] !== props.right) {"));
        assert!(rewritten.contains("$[2] = value;"));
        assert!(rewritten.contains("return value;"));
    }

    #[test]
    fn builds_single_slot_memoized_body_from_shape_without_generated_source() {
        let source = "function Foo(t0) { return null; }";
        let allocator = Allocator::default();
        let mut statements =
            parse_statements(&allocator, source_type_for_filename("fixture.jsx"), source).unwrap();
        let statement = statements.pop().unwrap();
        let ast::Statement::FunctionDeclaration(function) = statement else {
            panic!("expected function declaration");
        };

        let mut compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "let __memo; if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { __memo = props.value; $[0] = __memo; } else { __memo = $[0]; } let value = t0 === undefined ? __memo : t0; return value;",
            &["t0"],
            false,
        );
        compiled_function.generated_body = None;
        compiled_function.generated_body_shape =
            crate::reactive_scopes::codegen_reactive::GeneratedBodyShape::SingleSlotMemoizedReturn {
                value_name: "value".to_string(),
                value_kind: ast::VariableDeclarationKind::Let,
                temp_name: "t0".to_string(),
                memoized_bindings: vec![],
                memoized_assignments: vec![],
                memoized_expressions: vec![],
                memoized_expr: "props.value".to_string(),
            };
        compiled_function.needs_cache_import = true;
        compiled_function.cache_prologue =
            Some(crate::reactive_scopes::codegen_reactive::CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            });

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const $ = _c(1);"));
        assert!(rewritten.contains("let __memo;"));
        assert!(rewritten.contains("if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) {"));
        assert!(rewritten.contains("__memo = props.value;"));
        assert!(rewritten.contains("$[0] = __memo;"));
        assert!(rewritten.contains("let value = t0 === undefined ? __memo : t0;"));
        assert!(rewritten.contains("return value;"));
    }
}
