use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};
use oxc_syntax::identifier::is_identifier_name;

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

struct RenderedCompiledFunction {
    before_emit: String,
    replacement_src: String,
    next_source_start: u32,
    outlined_functions: Vec<RenderedOutlinedFunction>,
}

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

    let raw_result = super::raw::emit_module(args, compiled.clone());
    if !raw_result.transformed {
        return raw_result;
    }

    match try_emit_module(args, &compiled) {
        Ok(result) => result,
        Err(_) => raw_result,
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
    let mut rendered_prefix = String::new();
    for import_plan in &state.imports_to_insert {
        let statement = build_inserted_import_statement(builder, import_plan);
        let statement_source = codegen_statement_source(&allocator, state.source_type, &statement);
        body.push(statement);
        rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
        rendered_prefix.push('\n');
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
                let statement_source =
                    codegen_statement_source(&allocator, state.source_type, &statement);
                body.push(statement);
                rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
                rendered_prefix.push('\n');
            } else {
                body.push(stmt.clone_in(&allocator));
                let span = stmt.span();
                rendered_prefix.push_str(&args.source[span.start as usize..span.end as usize]);
                rendered_prefix.push('\n');
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
            rendered_prefix.push_str(&maybe_gated);
            rendered_prefix.push('\n');
            continue;
        }

        if stmt_compiled.len() == 1
            && stmt_compiled[0].start == span.start
            && stmt_compiled[0].end == span.end
            && let Some(statements) = try_lower_compiled_statement_ast(builder, stmt_compiled[0])
        {
            for statement in statements {
                let statement_source =
                    codegen_statement_source(&allocator, state.source_type, &statement);
                body.push(statement);
                rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
                rendered_prefix.push('\n');
            }
            continue;
        }

        if stmt_compiled.len() == 1
            && let Some(statements) = try_rewrite_compiled_statement_ast(
                builder,
                &allocator,
                state.source_type,
                args.source,
                stmt,
                stmt_compiled[0],
                &state,
            )
        {
            for statement in statements {
                let statement_source =
                    codegen_statement_source(&allocator, state.source_type, &statement);
                body.push(statement);
                rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
                rendered_prefix.push('\n');
            }
            continue;
        }

        let (rewritten_stmt, outlined_functions) = rewrite_statement_source(
            span.start as usize,
            span.end as usize,
            &stmt_compiled,
            &rendered_prefix,
            args.source,
            &state,
        )?;
        let rewritten_stmt = if let Some(name) = gate_name {
            maybe_gate_entrypoint_source(rewritten_stmt, name)
        } else {
            rewritten_stmt
        };
        body.extend(parse_statements(
            &allocator,
            state.source_type,
            allocator.alloc_str(&rewritten_stmt),
        )?);
        rendered_prefix.push_str(&rewritten_stmt);
        rendered_prefix.push('\n');
        for outlined in outlined_functions {
            if let Some(hir_function) = outlined.hir_function.as_ref()
                && let Some(statement) =
                    super::hir_to_ast::try_lower_function_declaration_ast(builder, hir_function)
            {
                let statement_source =
                    codegen_statement_source(&allocator, state.source_type, &statement);
                body.push(statement);
                rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
            } else {
                let source = format_outlined_function_source(
                    &outlined.name,
                    &outlined.params,
                    &outlined.body,
                );
                body.extend(parse_statements(
                    &allocator,
                    state.source_type,
                    allocator.alloc_str(&source),
                )?);
                rendered_prefix.push_str(&source);
            }
            rendered_prefix.push('\n');
        }
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
    let code = codegen_program(&program);
    let transformed = super::shared::normalize_for_transform_flag(&code)
        != super::shared::normalize_for_transform_flag(args.source_untransformed);

    Ok(CompileResult {
        transformed,
        code: if transformed {
            code
        } else {
            args.source_untransformed.to_string()
        },
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

fn rewrite_statement_source(
    stmt_start: usize,
    stmt_end: usize,
    compiled: &[&CompiledFunction],
    global_prefix: &str,
    source: &str,
    state: &AstRenderState,
) -> Result<(String, Vec<RenderedOutlinedFunction>), String> {
    let mut rewritten = String::new();
    let mut last_end = stmt_start;
    let mut outlined_functions = Vec::new();

    for cf in compiled {
        if (cf.start as usize) < last_end || (cf.end as usize) > stmt_end {
            return Err(format!(
                "invalid compiled span [{}..{}] for statement [{}..{}]",
                cf.start, cf.end, stmt_start, stmt_end
            ));
        }
        let local_before = source[last_end..cf.start as usize].to_string();
        let context_before = format!("{global_prefix}{}{}", rewritten, local_before);
        let rendered = render_compiled_function(cf, local_before, &context_before, source, state);
        rewritten.push_str(&rendered.before_emit);
        rewritten.push_str(&rendered.replacement_src);
        last_end = if (rendered.next_source_start as usize) > stmt_end {
            cf.end as usize
        } else {
            rendered.next_source_start as usize
        };
        outlined_functions.extend(rendered.outlined_functions);
    }

    rewritten.push_str(&source[last_end..stmt_end]);
    Ok((rewritten, outlined_functions))
}

fn try_rewrite_compiled_statement_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    source: &str,
    stmt: &ast::Statement<'_>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if state.gating_local_name.is_some()
        && cf.needs_cache_import
        && source[stmt.span().start as usize..stmt.span().end as usize].contains("FIXTURE_ENTRYPOINT")
    {
        return None;
    }

    let body_source = render_compiled_body_source(cf, state);
    let function_body =
        parse_compiled_function_body(allocator, source_type, cf, &body_source).ok()?;
    let compiled_params = cf.compiled_params.as_deref()?;
    let mut rewritten_stmt = stmt.clone_in(allocator);
    let rewritten = if let Some(gate_name) = state.gating_local_name.as_deref().filter(|_| cf.needs_cache_import) {
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

    let mut statements = builder.vec1(rewritten_stmt);
    for outlined in collect_rendered_outlined_functions(cf) {
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
        ast::Statement::ThrowStatement(throw_statement) => replace_compiled_function_in_expression_with_gate(
            builder,
            allocator,
            &mut throw_statement.argument,
            gate_name,
            cf,
            compiled_params,
            function_body,
        ),
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
        ast::ExportDefaultDeclarationKind::TSInstantiationExpression(
            instantiation_expression,
        ) => replace_compiled_function_in_expression_with_gate(
            builder,
            allocator,
            &mut instantiation_expression.expression,
            gate_name,
            cf,
            compiled_params,
            function_body,
        ),
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
        ast::Expression::SequenceExpression(sequence_expression) => {
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
    let conditional = make_gate_conditional_expression(builder, gate_name, span, consequent, alternate);
    match conditional {
        ast::Expression::FunctionExpression(function) => {
            ast::ExportDefaultDeclarationKind::FunctionExpression(function)
        }
        ast::Expression::ArrowFunctionExpression(arrow) => {
            ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
        }
        ast::Expression::CallExpression(call) => ast::ExportDefaultDeclarationKind::CallExpression(call),
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
        ast::Expression::TSAsExpression(ts_as) => ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as),
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
    let wrapper = format!(
        "{}function {}__codex_ast_body() {{\n{}\n}}",
        async_prefix, generator_prefix, body_source
    );
    let mut statements = parse_statements(allocator, source_type, allocator.alloc_str(&wrapper))?;
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

fn render_compiled_function(
    cf: &CompiledFunction,
    mut before_emit: String,
    context_before: &str,
    source: &str,
    state: &AstRenderState,
) -> RenderedCompiledFunction {
    let mut body = render_compiled_body_source(cf, state);
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }

    let body_for_emit = body.trim_end_matches('\n');
    let compiled_fn_src = if cf.is_arrow {
        let async_prefix = if cf.is_async { "async " } else { "" };
        format!(
            "{}({}) => {{\n{}\n}}",
            async_prefix, cf.params_str, body_for_emit
        )
    } else {
        let async_prefix = if cf.is_async { "async " } else { "" };
        let gen_prefix = if cf.is_generator { "*" } else { "" };
        format!(
            "{}function {}{}({}) {{\n{}\n}}",
            async_prefix, gen_prefix, cf.name, cf.params_str, body_for_emit
        )
    };

    let original_src_raw = source[cf.start as usize..cf.end as usize]
        .trim()
        .to_string();
    let preserve_original = super::shared::should_preserve_original_layout_for_equivalent_output(
        cf,
        &compiled_fn_src,
        &original_src_raw,
    );
    let mut replacement_src = if preserve_original {
        original_src_raw.clone()
    } else {
        compiled_fn_src.clone()
    };
    let mut next_source_start = cf.end;

    if let Some(gate_name) = state
        .gating_local_name
        .as_ref()
        .filter(|_| cf.needs_cache_import)
    {
        let gate_call = format!("{}()", gate_name);
        let has_parenthesized_arrow_body = cf.is_arrow && original_src_raw.contains("=> (");
        let original_src = super::shared::gated_uncompiled_function_source(source, cf);
        let before_trimmed_owned = before_emit.trim_end().to_string();
        let before_trimmed = before_trimmed_owned.as_str();
        let export_default_ctx = before_trimmed.ends_with("export default");
        let expression_ctx = cf.is_arrow || crate::pipeline::is_expression_context(before_trimmed);
        let before_ends_with_open_paren = before_trimmed.ends_with('(');

        replacement_src = if export_default_ctx && !cf.is_arrow && !cf.name.is_empty() {
            let prefix = &before_trimmed[..before_trimmed.len() - "export default".len()];
            before_emit = prefix.to_string();
            format!(
                "const {} = {} ? {} : {};\nexport default {};",
                cf.name, gate_call, compiled_fn_src, original_src, cf.name
            )
        } else if expression_ctx || export_default_ctx {
            format!("{} ? {} : {}", gate_call, compiled_fn_src, original_src)
        } else {
            let referenced_before_decl =
                crate::pipeline::has_early_binding_reference(context_before, &cf.name);
            if referenced_before_decl && !cf.name.is_empty() {
                let gate_result_name = format!("{}_result", gate_name);
                let optimized_name = format!("{}_optimized", cf.name);
                let unoptimized_name = format!("{}_unoptimized", cf.name);
                let optimized_fn = crate::pipeline::rename_function_declaration_name(
                    &compiled_fn_src,
                    &cf.name,
                    &optimized_name,
                );
                let unoptimized_fn = crate::pipeline::rename_function_declaration_name(
                    &original_src_raw,
                    &cf.name,
                    &unoptimized_name,
                );
                let param_count = crate::pipeline::count_param_slots(&cf.params_str);
                let wrapper_params = (0..param_count)
                    .map(|i| format!("arg{}", i))
                    .collect::<Vec<_>>()
                    .join(", ");
                let wrapper_args = wrapper_params.clone();
                format!(
                    "const {} = {};\n{}\n{}\nfunction {}({}) {{\nif ({}) return {}({});\nelse return {}({});\n}}",
                    gate_result_name,
                    gate_call,
                    optimized_fn,
                    unoptimized_fn,
                    cf.name,
                    wrapper_params,
                    gate_result_name,
                    optimized_name,
                    wrapper_args,
                    unoptimized_name,
                    wrapper_args
                )
            } else {
                format!(
                    "const {} = {} ? {} : {};",
                    cf.name, gate_call, compiled_fn_src, original_src
                )
            }
        };

        if !expression_ctx {
            if before_emit.trim().is_empty() {
                before_emit.clear();
            } else {
                before_emit = crate::pipeline::trim_trailing_blank_lines(&before_emit);
            }
        }

        let has_closing_paren_after = source[cf.end as usize..].trim_start().starts_with(')');
        if expression_ctx
            && before_ends_with_open_paren
            && !replacement_src.ends_with(')')
            && !has_closing_paren_after
        {
            replacement_src.push(')');
        }

        if cf.is_arrow && has_parenthesized_arrow_body {
            let trailing = &source[cf.end as usize..];
            if trailing.starts_with(";\n\nexport default")
                || trailing.starts_with(";\n\nexport const FIXTURE_ENTRYPOINT")
            {
                if !replacement_src.ends_with(';') {
                    replacement_src.push(';');
                }
                if (cf.end as usize) + 2 <= source.len() {
                    next_source_start = cf.end + 2;
                }
            }
        }
    }

    let outlined_functions = collect_rendered_outlined_functions(cf);

    RenderedCompiledFunction {
        before_emit,
        replacement_src,
        next_source_start,
        outlined_functions,
    }
}

fn render_compiled_body_source(cf: &CompiledFunction, state: &AstRenderState) -> String {
    let mut body = cf.generated_body.clone();
    if state.cache_import_name != "_c" {
        body = body.replacen("_c(", &format!("{}(", state.cache_import_name), 1);
    }
    if !cf.param_destructurings.is_empty() && !body.contains("=== undefined ?") {
        let pruned: Vec<String> = cf
            .param_destructurings
            .iter()
            .enumerate()
            .map(|(i, destructuring)| {
                let after: String = cf.param_destructurings[i + 1..].join("\n");
                let context = format!("{}\n{}", body, after);
                crate::pipeline::prune_unused_destructuring(destructuring, &context)
            })
            .collect();
        body = crate::pipeline::insert_param_destructurings(&body, &pruned);
    }
    if !cf.preserved_body_statements.is_empty() {
        body =
            crate::pipeline::insert_preserved_body_statements(&body, &cf.preserved_body_statements);
    }
    if !cf.directives.is_empty() {
        body = crate::pipeline::strip_directive_lines(&body, &cf.directives);
    }

    let mut prologue = String::new();
    if !cf.directives.is_empty() {
        let directives_str: String = cf
            .directives
            .iter()
            .map(|d| format!("  {};\n", d))
            .collect();
        prologue.push_str(&directives_str);
    }
    if cf.needs_instrument_forget {
        let rendered_name = if cf.name.is_empty() {
            "<anonymous>"
        } else {
            cf.name.as_str()
        };
        prologue.push_str(&format!(
            "  if (DEV && {})\n    {}(\"{}\", \"{}\");\n",
            state.should_instrument_ident,
            state.use_render_counter_ident,
            rendered_name,
            state.instrument_source_path
        ));
    }
    if !prologue.is_empty() {
        body = format!("{}{}", prologue, body);
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
    if cf.needs_hook_guards {
        body = crate::pipeline::maybe_align_hook_guard_name(&body, &state.hook_guard_ident);
    }
    if cf.needs_structural_check_import {
        body = crate::pipeline::maybe_align_structural_check_name(
            &body,
            &state.structural_check_ident,
        );
    }
    if cf.needs_lower_context_access && !state.lower_context_access_ident.is_empty() {
        body = crate::pipeline::maybe_align_lower_context_access_name(
            &body,
            &state.lower_context_access_imported,
            &state.lower_context_access_ident,
        );
    }
    body = crate::pipeline::insert_blank_lines_for_guarded_cache_init(&body);
    body
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
    cf: &CompiledFunction,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if !can_emit_compiled_statement_ast(cf) {
        return None;
    }
    let mut statements = builder.vec();
    statements.push(super::hir_to_ast::try_lower_function_declaration_ast(
        builder,
        cf.hir_function.as_ref()?,
    )?);
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

fn can_emit_compiled_statement_ast(cf: &CompiledFunction) -> bool {
    cf.body_payload == CompiledBodyPayload::LowerFromFinalHir
        && cf.is_function_declaration
        && !cf.needs_cache_import
        && cf.param_destructurings.is_empty()
        && cf.preserved_body_statements.is_empty()
        && !cf.needs_instrument_forget
        && !cf.needs_emit_freeze
        && !cf.needs_hook_guards
        && !cf.needs_structural_check_import
        && !cf.needs_lower_context_access
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

    use super::{
        AstRenderState, CompiledBodyPayload, CompiledFunction, CompiledParam,
        codegen_statement_source, maybe_gate_entrypoint_source, parse_statements,
        source_type_for_filename, try_rewrite_compiled_statement_ast,
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

    fn make_test_compiled_function(
        name: &str,
        start: u32,
        end: u32,
        generated_body: &str,
        params: &[&str],
        is_arrow: bool,
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
            original_params_str: params.join(", "),
            param_destructurings: vec![],
            is_async: false,
            is_generator: false,
            is_arrow,
            is_function_declaration: false,
            body_start: start,
            body_end: end,
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
            compiled_function,
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
        assert!(
            rewritten.contains(
                "export default React.forwardRef(gate() ? function FancyButton(props, ref) {"
            )
        );
        assert!(rewritten.contains(": function FancyButton(props, ref) {"));
    }
}
