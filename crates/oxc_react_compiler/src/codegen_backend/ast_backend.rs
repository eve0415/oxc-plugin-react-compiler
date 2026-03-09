use oxc_allocator::{Allocator, CloneIn, Dummy};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};
use oxc_syntax::identifier::is_identifier_name;
use oxc_syntax::operator::LogicalOperator;

use crate::CompileResult;

use super::{CompiledBodyPayload, CompiledFunction, CompiledParam, ModuleEmitArgs};

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
    params: String,
    body: String,
    hir_function: Option<crate::hir::types::HIRFunction>,
}

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    let compiled = compiled
        .into_iter()
        .map(|mut compiled_function| {
            if compiled_function.body_payload == CompiledBodyPayload::LowerFromFinalHir
                && !can_emit_compiled_statement_ast(&compiled_function)
                && let Some(hir_function) = compiled_function.hir_function.as_ref()
                && let Some(lowered_body) = super::hir_to_ast::try_lower_function_body(hir_function)
            {
                compiled_function.generated_body = lowered_body;
            }
            compiled_function
        })
        .collect::<Vec<_>>();

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

fn compute_transform_state(source_type: SourceType, output_code: &str, source_untransformed: &str) -> bool {
    let output = normalize_module_for_transform_flag(source_type, output_code);
    let source = normalize_module_for_transform_flag(source_type, source_untransformed);
    if output.normalized == source.normalized {
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
        let is_nested = statements.iter().any(|span| {
            comment.span.start >= span.start && comment.span.end <= span.end
        });

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
                .map(|name| maybe_gate_entrypoint_source(original_stmt.clone(), name))
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

        let body_source = render_compiled_body_source(cf, state);
        let mut function_body =
            parse_compiled_function_body(allocator, source_type, cf, &body_source).ok()?;
        apply_preserved_directives(builder, &mut function_body, cf);
        prepend_compiled_body_prefix_statements(
            builder,
            allocator,
            source_type,
            &mut function_body,
            cf,
            Some(&state.cache_import_name),
        )?;
        prepend_instrument_forget_statement(builder, allocator, &mut function_body, cf, state);
        align_runtime_identifier_references(builder, &mut function_body, cf, state);
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
    for outlined in outlined_functions {
        if let Some(hir_function) = outlined.hir_function.as_ref()
            && let Some(statement) =
                super::hir_to_ast::try_lower_function_declaration_ast(builder, hir_function)
        {
            statements.push(statement);
            continue;
        }

        let source =
            format_outlined_function_source(&outlined.name, &outlined.params, &outlined.body);
        statements
            .extend(parse_statements(allocator, source_type, allocator.alloc_str(&source)).ok()?);
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

    let body_source = render_compiled_body_source(cf, state);
    let mut function_body =
        parse_compiled_function_body(allocator, state.source_type, cf, &body_source).ok()?;
    apply_preserved_directives(builder, &mut function_body, cf);
    prepend_compiled_body_prefix_statements(
        builder,
        allocator,
        state.source_type,
        &mut function_body,
        cf,
        Some(&state.cache_import_name),
    )?;
    prepend_instrument_forget_statement(builder, allocator, &mut function_body, cf, state);
    align_runtime_identifier_references(builder, &mut function_body, cf, state);
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
    let param_count = crate::pipeline::count_param_slots(&cf.params_str);
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

fn parse_compiled_function_body<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
    body_source: &str,
) -> Result<ast::FunctionBody<'a>, String> {
    let async_prefix = if cf.is_async { "async " } else { "" };
    let generator_prefix = if cf.is_generator { "*" } else { "" };
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

fn render_compiled_body_source(cf: &CompiledFunction, state: &AstRenderState) -> String {
    let mut body = cf.generated_body.clone();
    if !cf.directives.is_empty() {
        body = crate::pipeline::strip_directive_lines(&body, &cf.directives);
    }
    if cf.needs_emit_freeze {
        let freeze_name = if cf.name.is_empty() {
            "<anonymous>"
        } else {
            cf.name.as_str()
        };
        body = crate::pipeline::maybe_apply_emit_freeze_to_cache_stores(
            &body,
            &state.make_read_only_ident,
            freeze_name,
        );
    }
    body = crate::pipeline::insert_blank_lines_for_guarded_cache_init(&body);
    body
}

fn apply_preserved_directives<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
) {
    if cf.directives.is_empty() {
        return;
    }
    body.directives = builder.vec_from_iter(
        cf.directives
            .iter()
            .filter_map(|directive| build_directive(builder, directive)),
    );
}

fn build_directive<'a>(
    builder: AstBuilder<'a>,
    directive: &str,
) -> Option<ast::Directive<'a>> {
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

fn prepend_compiled_body_prefix_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    cache_import_name: Option<&str>,
) -> Option<()> {
    let prefix_source = build_compiled_body_prefix_source(cf);
    if prefix_source.is_empty() {
        return Some(());
    }

    let prefix_statements = parse_statements(
        allocator,
        source_type,
        allocator.alloc_str(prefix_source.as_str()),
    )
    .ok()?;
    let insert_idx = cache_import_name
        .and_then(|cache_import_name| find_cache_initializer_index(&body.statements, cache_import_name))
        .map_or(0, |index| index + 1);
    let mut statements = builder.vec();
    statements.extend(
        body.statements[..insert_idx]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    statements.extend(prefix_statements);
    statements.extend(
        body.statements[insert_idx..]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
    Some(())
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
    let statement = builder.statement_if(
        SPAN,
        test,
        builder.statement_expression(SPAN, call),
        None,
    );

    let mut statements = builder.vec1(statement);
    statements.extend(body.statements.iter().map(|statement| statement.clone_in(allocator)));
    body.statements = statements;
}

fn collect_rendered_outlined_functions(cf: &CompiledFunction) -> Vec<RenderedOutlinedFunction> {
    cf.outlined_functions
        .iter()
        .map(|(fn_name, fn_params, fn_body)| {
            let hir_function = cf
                .hir_outlined_functions
                .iter()
                .find(|(outlined_name, _)| outlined_name == fn_name)
                .map(|(_, hir_function)| hir_function.clone());
            RenderedOutlinedFunction {
                name: fn_name.clone(),
                params: fn_params.clone(),
                body: fn_body.clone(),
                hir_function,
            }
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
    statements.push(function_statement);
    for (outlined_name, _, _) in &cf.outlined_functions {
        let hir_function = cf
            .hir_outlined_functions
            .iter()
            .find(|(hir_name, _)| hir_name == outlined_name)
            .map(|(_, hir_function)| hir_function)?;
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

struct IdentifierReferenceRenamer<'a, 'rename> {
    builder: AstBuilder<'a>,
    cache_import_name: Option<&'rename str>,
    renames: Vec<(&'rename str, &'rename str)>,
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

fn build_compiled_body_prefix_source(cf: &CompiledFunction) -> String {
    let mut prefix_source = String::new();
    if !cf.param_destructurings.is_empty() && !cf.generated_body.contains("=== undefined ?") {
        for (index, destructuring) in cf.param_destructurings.iter().enumerate() {
            let after: String = cf.param_destructurings[index + 1..].join("\n");
            let context = format!("{}\n{}", cf.generated_body, after);
            let pruned = crate::pipeline::prune_unused_destructuring(destructuring, &context);
            if pruned.trim().is_empty() {
                continue;
            }
            prefix_source.push_str(pruned.trim_end());
            prefix_source.push('\n');
        }
    }
    for statement in &cf.preserved_body_statements {
        if statement.trim().is_empty() {
            continue;
        }
        prefix_source.push_str(statement.trim_end());
        prefix_source.push('\n');
    }
    prefix_source
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
        && !cf.needs_emit_freeze
}

fn maybe_gate_entrypoint_source(source: String, gate_name: &str) -> String {
    crate::pipeline::gate_fixture_entrypoint_arrows(source, gate_name)
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

#[cfg(test)]
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

fn format_outlined_function_source(name: &str, params: &str, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        format!("function {}({}) {{}}", name, params)
    } else {
        format!("function {}({}) {{\n{}\n}}", name, params, trimmed)
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
    use oxc_allocator::Allocator;
    use oxc_ast::{AstBuilder, ast};
    use oxc_span::SourceType;

    use super::{
        AstRenderState, CompiledBodyPayload, CompiledFunction, CompiledParam,
        codegen_statement_source, compute_transform_state, maybe_gate_entrypoint_source,
        normalize_generated_body_flow_cast_marker_calls, parse_statements,
        restore_flow_cast_marker_calls, source_type_for_filename,
        try_rewrite_compiled_statement_ast,
    };

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
    fn rewrites_instrument_forget_prefix_as_ast_statement() {
        let source = r#"function Component(x) {
  return x;
}"#;
        let mut compiled_function =
            make_test_compiled_function("Component", 0, source.len() as u32, "return x;", &["x"], false);
        compiled_function.needs_instrument_forget = true;
        let state = AstRenderState {
            should_instrument_ident: "shouldInstrument".to_string(),
            use_render_counter_ident: "useRenderCounter".to_string(),
            instrument_source_path: "fixture.jsx".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };

        let rewritten =
            rewrite_single_statement_for_test_with_state("fixture.jsx", source, &compiled_function, state);

        assert!(rewritten.contains("if (DEV && shouldInstrument)"));
        assert!(rewritten.contains("useRenderCounter(\"Component\", \"fixture.jsx\")"));
    }

    #[test]
    fn rewrites_generated_prefix_statements_as_ast() {
        let source = r#"const FancyButton = (props) => null;"#;
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
        compiled_function.param_destructurings = vec!["const value = props.value;".to_string()];
        compiled_function.preserved_body_statements = vec!["track(value);".to_string()];

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const value = props.value;"));
        assert!(rewritten.contains("track(value);"));
        assert!(rewritten.contains("return props;"));
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

        let rewritten =
            rewrite_single_statement_for_test_with_state("fixture.jsx", source, &compiled_function, state);

        assert!(rewritten.contains("const cache = _cache(1);"));
        assert!(rewritten.contains("hookGuard(cache, loweredContext(structuralCheck));"));
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
            generated_body: generated_body.to_string(),
            body_payload: CompiledBodyPayload::GeneratedString,
            needs_cache_import: false,
            params_str: params.join(", "),
            compiled_params: Some(
                params
                    .iter()
                    .map(|name| CompiledParam {
                        name: (*name).to_string(),
                        is_rest: false,
                    })
                    .collect(),
            ),
            param_destructurings: vec![],
            is_async: false,
            is_generator: false,
            is_function_declaration: false,
            directives: vec![],
            preserved_body_statements: vec![],
            hir_function: None,
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
        let output = maybe_gate_entrypoint_source(input, "gate");
        assert!(output.contains("fn: gate() ? () =>{} : () =>{}"));
        assert!(output.contains("useHook: gate() ? () =>{} : () =>{}"));
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
        compiled_function.params_str = "t0".to_string();
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
}
