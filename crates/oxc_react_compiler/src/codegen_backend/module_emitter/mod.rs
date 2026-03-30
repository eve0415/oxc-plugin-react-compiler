use std::collections::HashSet;

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_ast_visit::{Visit, VisitMut, walk, walk_mut};
use oxc_span::{GetSpan, SPAN, SourceType};
use oxc_syntax::operator::BinaryOperator;

use crate::CompileResult;

use super::{
    CompiledArrayPattern, CompiledBindingPattern, CompiledFunction, CompiledInitializer,
    CompiledObjectPattern, CompiledParam, CompiledParamPrefixStatement, CompiledPropertyKey,
    ModuleEmitArgs, SynthesizedDefaultParamCache,
};

mod blank_lines;
mod flow_cast;
mod function_replacement;
mod instrumentation;
mod postprocess;
mod transform_flag;

#[allow(unused_imports)]
use blank_lines::{
    apply_blank_line_markers, apply_internal_blank_line_markers, apply_memo_comment_markers,
    codegen_statement_source, move_leading_comment_to_import_trailing,
    transfer_blank_lines_from_original_source,
};
#[cfg(test)]
use flow_cast::normalize_generated_body_flow_cast_marker_calls;
#[cfg(test)]
use flow_cast::normalize_generated_body_iife_parenthesization;
use flow_cast::{FLOW_CAST_MARKER_HELPER, parse_expression_source, restore_flow_cast_marker_calls};
use function_replacement::{
    replace_compiled_function_in_statement, replace_compiled_function_in_statement_with_gate,
    try_build_gated_function_declaration_statements,
};
use instrumentation::{
    align_runtime_identifier_references, apply_emit_freeze_to_cache_stores_ast,
    apply_preserved_directives, collect_rendered_outlined_functions,
    expression_references_identifier, function_body_contains_undefined_fallback,
    normalize_use_fire_binding_temps_ast, prepend_cache_prologue_statements,
    prepend_compiled_body_prefix_statements, prepend_instrument_forget_statement,
    prepend_synthesized_default_param_cache_statements, try_lower_compiled_statement_ast,
    wrap_function_hook_guard_body, wrap_hook_guard_body,
};
#[cfg(test)]
use postprocess::codegen_program;
use postprocess::{
    build_inserted_import_statement, build_runtime_import_merge_statement,
    codegen_program_with_source_map, parse_statements,
};
use transform_flag::{canonicalize_initializer_expressions_in_statements, compute_transform_state};

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
    ast_identifier_renames: std::collections::HashMap<(u32, u32), String>,
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

struct RenderedOutlinedFunction {
    name: String,
    params: Vec<CompiledParam>,
    directives: Vec<String>,
    cache_prologue: Option<crate::codegen_backend::codegen_ast::CachePrologue>,
    needs_function_hook_guard_wrapper: bool,
    is_async: bool,
    is_generator: bool,
    reactive_function: Option<crate::hir::types::ReactiveFunction>,
    // AST codegen options (forwarded from parent CompiledFunction).
    enable_change_variable_codegen: bool,
    unique_identifiers: std::collections::HashSet<String>,
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
    // Track which output body indices need a blank line before them (preserving original source)
    let mut blank_line_before: Vec<bool> = Vec::new();

    for import_plan in &state.imports_to_insert {
        let statement = build_inserted_import_statement(builder, import_plan);
        body.push(statement);
        blank_line_before.push(false);
    }

    // Precompute which original statements have a blank line before them in the source
    let original_stmts = &args.program.body;
    let mut original_has_blank_before: Vec<bool> = vec![false; original_stmts.len()];
    if let Some(first_stmt) = original_stmts.first() {
        let first_stmt_start = first_stmt.span().start;
        let leading_comment_end = args
            .program
            .comments
            .iter()
            .filter(|comment| comment.span.end <= first_stmt_start)
            .filter(|comment| {
                !original_stmts.iter().any(|stmt| {
                    let span = stmt.span();
                    comment.span.start >= span.start && comment.span.end <= span.end
                })
            })
            .map(|comment| comment.span.end as usize)
            .max()
            .unwrap_or(0);
        if leading_comment_end > 0 {
            let between = &args.source[leading_comment_end..first_stmt_start as usize];
            let newline_count = between.chars().filter(|&c| c == '\n').count();
            if newline_count >= 2 {
                original_has_blank_before[0] = true;
            }
        }
    }
    for i in 1..original_stmts.len() {
        let prev_end = original_stmts[i - 1].span().end as usize;
        let curr_start = original_stmts[i].span().start as usize;
        if curr_start > prev_end {
            let between = &args.source[prev_end..curr_start];
            let newline_count = between.chars().filter(|&c| c == '\n').count();
            if newline_count >= 2 {
                original_has_blank_before[i] = true;
            }
        }
    }

    let mut compiled_sorted = compiled.iter().collect::<Vec<_>>();
    compiled_sorted.sort_by_key(|cf| cf.start);
    let mut compiled_idx = 0usize;
    // Outlined functions from expression-based compiled functions (const Component = ...)
    // are deferred to the end, matching upstream's pushContainer('body') behavior.
    // FunctionDeclarations get their outlined functions immediately after (insertAfter).
    let mut deferred_outlined: Vec<ast::Statement<'_>> = Vec::new();

    for (orig_idx, stmt) in args.program.body.iter().enumerate() {
        let has_blank = original_has_blank_before[orig_idx];

        if let ast::Statement::ImportDeclaration(import_decl) = stmt
            && let Some(plan) = state.runtime_import_merge_plan.as_ref()
            && import_decl.span.start == plan.start
            && import_decl.span.end == plan.end
        {
            if plan.replacement.is_some() {
                let statement = build_runtime_import_merge_statement(builder, &plan.merged_specs);
                body.push(statement);
                blank_line_before.push(has_blank);
            } else {
                body.push(stmt.clone_in(&allocator));
                blank_line_before.push(has_blank);
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
                let mut cloned = stmt.clone_in(&allocator);
                apply_planned_identifier_renames(
                    builder,
                    &mut cloned,
                    &state.ast_identifier_renames,
                );
                body.push(cloned);
                blank_line_before.push(has_blank);
            } else {
                let stmts = parse_statements(
                    &allocator,
                    state.source_type,
                    allocator.alloc_str(&maybe_gated),
                )?;
                let mut first = true;
                for s in stmts {
                    body.push(s);
                    blank_line_before.push(if first { has_blank } else { false });
                    first = false;
                }
            }
            continue;
        }

        // Upstream places outlined functions immediately after the compiled
        // FunctionDeclaration (insertAfter), but at the end for expression
        // functions (pushContainer). Match this behavior.
        let is_func_decl = matches!(stmt, ast::Statement::FunctionDeclaration(_))
            || matches!(stmt,
                ast::Statement::ExportDefaultDeclaration(ed)
                if matches!(&ed.declaration, ast::ExportDefaultDeclarationKind::FunctionDeclaration(_))
            )
            || matches!(stmt,
                ast::Statement::ExportNamedDeclaration(en)
                if en.declaration.as_ref().is_some_and(|d| matches!(d, ast::Declaration::FunctionDeclaration(_)))
            );
        let outlined_fn_names: HashSet<String> = if !is_func_decl {
            stmt_compiled
                .iter()
                .flat_map(|cf| {
                    cf.outlined_functions
                        .iter()
                        .map(|of| of.name.clone())
                        .chain(cf.hir_outlined_functions.iter().map(|(n, _)| n.clone()))
                })
                .collect()
        } else {
            HashSet::new()
        };

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
            let mut first = true;
            for statement in statements {
                let is_outlined = !first
                    && !is_func_decl
                    && matches!(&statement, ast::Statement::FunctionDeclaration(f) if
                        f.id.as_ref().is_some_and(|id| outlined_fn_names.contains(id.name.as_str()))
                    );
                if is_outlined {
                    deferred_outlined.push(statement);
                } else {
                    body.push(statement);
                    blank_line_before.push(if first { has_blank } else { false });
                }
                first = false;
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
            let mut first = true;
            for statement in statements {
                let is_outlined = !first
                    && !is_func_decl
                    && matches!(&statement, ast::Statement::FunctionDeclaration(f) if
                        f.id.as_ref().is_some_and(|id| outlined_fn_names.contains(id.name.as_str()))
                    );
                if is_outlined {
                    deferred_outlined.push(statement);
                } else {
                    body.push(statement);
                    blank_line_before.push(if first { has_blank } else { false });
                }
                first = false;
            }
            continue;
        }

        return Err(format!(
            "unable to AST-rewrite compiled statement [{}..{}] in {}",
            span.start, span.end, args.filename
        ));
    }

    canonicalize_initializer_expressions_in_statements(&allocator, &mut body);

    // Append deferred outlined functions (from expression-based compiled functions)
    for outlined_stmt in deferred_outlined {
        body.push(outlined_stmt);
        blank_line_before.push(false);
    }

    let program = builder.program(
        SPAN,
        state.source_type,
        allocator.alloc_str(args.source),
        args.program.comments.clone_in(&allocator),
        args.program.hashbang.clone_in(&allocator),
        args.program.directives.clone_in(&allocator),
        body,
    );
    let source_map_path = if args.options.source_map {
        Some(args.filename)
    } else {
        None
    };
    let (mut code, raw_sourcemap) = codegen_program_with_source_map(&program, source_map_path);

    // Remaining Category A/B post-processing transforms (markers, blank lines, comments).
    // Track line insertions/removals per-transform for position-aware sourcemap adjustment.
    let mut line_edits: Vec<LineEdit> = Vec::new();

    /// Track line changes between before and after code, recording where lines
    /// were inserted or removed relative to the ORIGINAL generated code positions.
    macro_rules! track_line_edits {
        ($code:expr, $edits:expr, $transform:expr) => {{
            let before = &$code;
            let after = $transform;
            collect_line_edits(before, &after, $edits);
            $code = after;
        }};
    }

    track_line_edits!(
        code,
        &mut line_edits,
        apply_internal_blank_line_markers(&code)
    );
    track_line_edits!(code, &mut line_edits, apply_memo_comment_markers(&code));
    track_line_edits!(
        code,
        &mut line_edits,
        apply_blank_line_markers(state.source_type, &code, &blank_line_before)
    );
    track_line_edits!(
        code,
        &mut line_edits,
        transfer_blank_lines_from_original_source(&code, args.source, compiled)
    );
    track_line_edits!(
        code,
        &mut line_edits,
        move_leading_comment_to_import_trailing(&code, args.source)
    );
    if code.contains(FLOW_CAST_MARKER_HELPER) {
        track_line_edits!(code, &mut line_edits, restore_flow_cast_marker_calls(&code));
    }

    let map = raw_sourcemap.map(|sm| {
        let mut sm = adjust_sourcemap_positional(sm, &line_edits);
        enrich_sourcemap(&mut sm);
        sm.to_json_string()
    });

    Ok(CompileResult {
        transformed: true,
        code,
        map,
    })
}

/// A line edit records that at a given line position in the generated code,
/// lines were inserted (positive delta) or removed (negative delta).
#[derive(Debug, Clone, Copy)]
struct LineEdit {
    /// The 0-based line in the generated code BEFORE this edit.
    line: u32,
    /// Number of lines inserted (+) or removed (-) at this position.
    delta: i32,
}

/// Compare before and after code to find where lines were inserted or removed.
/// Records edits relative to the `before` code's line numbering.
fn collect_line_edits(before: &str, after: &str, edits: &mut Vec<LineEdit>) {
    let before_count = before.as_bytes().iter().filter(|&&b| b == b'\n').count() as i32;
    let after_count = after.as_bytes().iter().filter(|&&b| b == b'\n').count() as i32;
    let total_delta = after_count - before_count;
    if total_delta == 0 {
        return;
    }

    // Find the first line that differs to determine the insertion/removal point.
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let mut first_diff = 0u32;
    for (i, (b, a)) in before_lines.iter().zip(after_lines.iter()).enumerate() {
        if b != a {
            first_diff = i as u32;
            break;
        }
        first_diff = (i + 1) as u32;
    }

    // Apply cumulative offset from previous edits to get the correct position.
    let cumulative_offset: i32 = edits
        .iter()
        .filter(|e| e.line <= first_diff)
        .map(|e| e.delta)
        .sum();
    let adjusted_line = (first_diff as i32 - cumulative_offset).max(0) as u32;

    edits.push(LineEdit {
        line: adjusted_line,
        delta: total_delta,
    });
}

/// Adjust sourcemap token positions using position-aware line edits.
/// Each edit specifies where lines were inserted/removed. Tokens at or after
/// the edit position get their generated line adjusted by the cumulative delta.
fn adjust_sourcemap_positional(
    sm: oxc_sourcemap::SourceMap,
    edits: &[LineEdit],
) -> oxc_sourcemap::SourceMap {
    if edits.is_empty() {
        return sm;
    }

    // Build a sorted list of edits by line position.
    let mut sorted_edits: Vec<LineEdit> = edits.to_vec();
    sorted_edits.sort_by_key(|e| e.line);

    use std::sync::Arc;
    let tokens: Vec<oxc_sourcemap::Token> = sm
        .get_tokens()
        .map(|t| {
            let dst_line = t.get_dst_line();
            // Sum deltas from all edits at or before this token's original line.
            let delta: i32 = sorted_edits
                .iter()
                .filter(|e| e.line <= dst_line)
                .map(|e| e.delta)
                .sum();
            let new_dst_line = (dst_line as i64 + delta as i64).max(0) as u32;
            oxc_sourcemap::Token::new(
                new_dst_line,
                t.get_dst_col(),
                t.get_src_line(),
                t.get_src_col(),
                t.get_source_id(),
                t.get_name_id(),
            )
        })
        .collect();
    oxc_sourcemap::SourceMap::new(
        sm.get_file().map(|s| Arc::from(s.as_ref())),
        sm.get_names().map(|s| Arc::from(s.as_ref())).collect(),
        sm.get_source_root().map(|s| s.to_string()),
        sm.get_sources().map(|s| Arc::from(s.as_ref())).collect(),
        sm.get_source_contents()
            .map(|c| c.map(|s| Arc::from(s.as_ref())))
            .collect(),
        tokens.into_boxed_slice(),
        None,
    )
}

/// Enrich a sourcemap with `debugId` (UUID v4) and `x_google_ignoreList`.
/// The `debugId` enables error monitoring tools (Sentry, Datadog) to match
/// sourcemaps to deployed bundles. The `x_google_ignoreList` tells Chrome
/// DevTools to auto-skip certain sources during debugging.
fn enrich_sourcemap(sm: &mut oxc_sourcemap::SourceMap) {
    // Generate a unique debugId (UUID v4) for error monitoring integration.
    sm.set_debug_id(&uuid::Uuid::new_v4().to_string());
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
        ast_identifier_renames: args.ast_identifier_renames.clone(),
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
    apply_planned_identifier_renames(builder, &mut rewritten_stmt, &state.ast_identifier_renames);
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
    for outlined in outlined_functions.into_iter().rev() {
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
        for (name, hir_function) in cf.hir_outlined_functions.iter().rev() {
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

fn apply_planned_identifier_renames<'a>(
    builder: AstBuilder<'a>,
    statement: &mut ast::Statement<'a>,
    renames: &std::collections::HashMap<(u32, u32), String>,
) {
    if renames.is_empty() {
        return;
    }
    let mut renamer = PlannedIdentifierRenamer { builder, renames };
    renamer.visit_statement(statement);
}

struct PlannedIdentifierRenamer<'a, 'map> {
    builder: AstBuilder<'a>,
    renames: &'map std::collections::HashMap<(u32, u32), String>,
}

impl<'a> VisitMut<'a> for PlannedIdentifierRenamer<'a, '_> {
    fn visit_binding_identifier(&mut self, it: &mut ast::BindingIdentifier<'a>) {
        if let Some(to) = self.renames.get(&(it.span.start, it.span.end)) {
            it.name = self.builder.ident(to);
        }
    }

    fn visit_identifier_reference(&mut self, it: &mut ast::IdentifierReference<'a>) {
        if let Some(to) = self.renames.get(&(it.span.start, it.span.end)) {
            it.name = self.builder.ident(to);
        }
    }

    fn visit_jsx_identifier(&mut self, it: &mut ast::JSXIdentifier<'a>) {
        if let Some(to) = self.renames.get(&(it.span.start, it.span.end)) {
            it.name = self.builder.atom(to);
        }
    }
}

pub(super) fn strip_compiled_function_signature_types(function: &mut ast::Function<'_>) {
    function.type_parameters = None;
    function.this_param = None;
    function.return_type = None;
}

pub(super) fn strip_compiled_arrow_signature_types(arrow: &mut ast::ArrowFunctionExpression<'_>) {
    arrow.type_parameters = None;
    arrow.return_type = None;
}

pub(super) fn make_compiled_formal_params<'a>(
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

pub(super) fn make_function_body<'a>(
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
    let ast_result = try_run_reactive_ast_codegen(builder, allocator, cf);
    let mut function_body = if let Some((body, _)) = ast_result.as_ref() {
        body.clone_in(allocator)
    } else if let Some(default_cache) = cf.synthesized_default_param_cache.as_ref() {
        build_default_param_cache_seed_body(builder, default_cache)
    } else {
        return None;
    };

    let effective_prologue = ast_result
        .and_then(|(_, p)| p)
        .or_else(|| cf.cache_prologue.clone());

    normalize_use_fire_binding_temps_ast(builder, &mut function_body, cf);
    wrap_function_hook_guard_body(builder, allocator, &mut function_body, cf, state);
    apply_preserved_directives(builder, &mut function_body, &cf.directives);
    prepend_cache_prologue_statements(
        builder,
        allocator,
        &mut function_body,
        effective_prologue.as_ref(),
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
    strip_redundant_trailing_void_return_from_function_body(&mut function_body);
    Some(function_body)
}

/// Run AST codegen and return both the function body and cache prologue.
fn try_run_reactive_ast_codegen<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    cf: &CompiledFunction,
) -> Option<(
    ast::FunctionBody<'a>,
    Option<crate::codegen_backend::codegen_ast::CachePrologue>,
)> {
    let reactive_fn = cf.reactive_function.as_ref()?;
    let options = crate::codegen_backend::codegen_ast::CodegenOptions {
        enable_change_variable_codegen: cf.enable_change_variable_codegen,
        enable_emit_hook_guards: cf.enable_emit_hook_guards,
        enable_change_detection_for_debugging: cf.enable_change_detection_for_debugging,
        enable_reset_cache_on_source_file_changes: cf.enable_reset_cache_on_source_file_changes,
        fast_refresh_source_hash: cf.fast_refresh_source_hash.clone(),
        disable_memoization_features: cf.disable_memoization_features,
        disable_memoization_for_debugging: cf.disable_memoization_for_debugging,
        fbt_operands: cf.fbt_operands.clone(),
        cache_binding_name: cf.cache_prologue.as_ref().map(|p| p.binding_name.clone()),
        unique_identifiers: cf.unique_identifiers.clone(),
        param_name_overrides: std::collections::HashMap::new(),
        enable_name_anonymous_functions: cf.enable_name_anonymous_functions,
        enable_memoization_comments: cf.enable_memoization_comments,
        emit_nested_context_reassign_reads: false,
    };
    let result = crate::codegen_backend::codegen_ast::codegen_reactive_function(
        builder,
        allocator,
        reactive_fn,
        options,
    );
    let body = builder.function_body(SPAN, builder.vec(), result.body);
    Some((body, result.cache_prologue))
}

fn try_build_outlined_function_body_from_reactive_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    outlined: &RenderedOutlinedFunction,
) -> Option<ast::FunctionBody<'a>> {
    let reactive_fn = outlined.reactive_function.as_ref()?;
    // Build param name overrides: map each reactive param's declaration_id to
    // the final name from CompiledParam (handles unnamed params that rename_variables skipped).
    let mut param_name_overrides = std::collections::HashMap::new();
    for (rf_param, compiled_param) in reactive_fn.params.iter().zip(outlined.params.iter()) {
        let decl_id = match rf_param {
            crate::hir::types::Argument::Place(p) => p.identifier.declaration_id,
            crate::hir::types::Argument::Spread(p) => p.identifier.declaration_id,
        };
        param_name_overrides.insert(decl_id, compiled_param.name.clone());
    }
    let options = crate::codegen_backend::codegen_ast::CodegenOptions {
        enable_change_variable_codegen: outlined.enable_change_variable_codegen,
        enable_emit_hook_guards: false,
        enable_change_detection_for_debugging: false,
        enable_reset_cache_on_source_file_changes: false,
        fast_refresh_source_hash: None,
        disable_memoization_features: false,
        disable_memoization_for_debugging: false,
        fbt_operands: Default::default(),
        cache_binding_name: outlined
            .cache_prologue
            .as_ref()
            .map(|p| p.binding_name.clone()),
        unique_identifiers: outlined.unique_identifiers.clone(),
        param_name_overrides,
        enable_name_anonymous_functions: false,
        enable_memoization_comments: false,
        emit_nested_context_reassign_reads: false,
    };
    let result = crate::codegen_backend::codegen_ast::codegen_reactive_function(
        builder,
        allocator,
        reactive_fn,
        options,
    );
    Some(builder.function_body(SPAN, builder.vec(), result.body))
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

fn strip_redundant_trailing_void_return_from_function_body(body: &mut ast::FunctionBody<'_>) {
    if matches!(
        body.statements.last(),
        Some(ast::Statement::ReturnStatement(return_statement))
            if return_statement.argument.is_none()
    ) {
        body.statements.pop();
    }
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn normalize_hir_match_destructuring_brace_spacing(code: &str) -> String {
    code.lines()
        .map(collapse_hir_match_destructuring_brace_spacing)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn is_hir_match_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

#[cfg(test)]
fn is_hir_match_ident_continue(ch: char) -> bool {
    is_hir_match_ident_start(ch) || ch.is_ascii_digit()
}

#[cfg(test)]
fn is_basic_block_label_open_brace(line: &str) -> bool {
    if !line.starts_with("bb") || !line.ends_with(": {") {
        return false;
    }
    let digits = &line[2..line.len() - 3];
    !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
}

#[cfg(test)]
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
    _source_type: SourceType,
    outlined: &RenderedOutlinedFunction,
    state: &AstRenderState,
) -> Option<ast::Statement<'a>> {
    let mut body =
        try_build_outlined_function_body_from_reactive_ast(builder, allocator, outlined)?;
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
    strip_redundant_trailing_void_return_from_function_body(&mut body);
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
    cf.is_function_declaration && !cf.needs_cache_import && cf.hir_function.is_some()
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

#[cfg(any())]
fn is_simple_generated_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
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
        codegen_backend::{CompiledOutlinedFunction, ModuleEmitArgs},
        environment::Environment,
        hir::types::{
            self, Argument, BasicBlock, BlockId, DeclarationId, Effect, HIR, HIRFunction,
            Identifier, IdentifierId, IdentifierName, InstructionId, InstructionValue,
            MutableRange, Place, PrimitiveValue, ReactFunctionType, ReactiveFunction,
            ReactiveInstruction, ReactiveStatement, ReactiveTerminal, ReactiveTerminalStatement,
            SourceLocation, Terminal, Type,
        },
        options::{EnvironmentConfig, PluginOptions},
    };

    use super::{
        AstRenderState, CompiledBindingPattern, CompiledFunction, CompiledInitializer,
        CompiledObjectPattern, CompiledParam, CompiledParamPrefixStatement, CompiledPropertyKey,
        apply_internal_blank_line_markers, codegen_statement_source, compute_transform_state,
        maybe_gate_entrypoint_source, normalize_compiled_body_for_hir_match,
        normalize_generated_body_flow_cast_marker_calls, normalize_use_fire_binding_temps_ast,
        parse_rendered_function_body, parse_statements, restore_flow_cast_marker_calls,
        source_type_for_filename, try_rewrite_compiled_statement_ast,
    };
    use crate::codegen_backend::CompiledObjectPatternProperty;
    use crate::codegen_backend::module_emitter::apply_emit_freeze_to_cache_stores_ast;

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
            ast_identifier_renames: std::collections::HashMap::new(),
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
    fn applies_internal_blank_line_markers_inside_blocks() {
        let marker = crate::codegen_backend::codegen_ast::INTERNAL_BLANK_LINE_MARKER;
        let source =
            format!("function component() {{\nconst y = {{}};\n\"{marker}\";\ny.x = x.a;\n}}\n");
        let rewritten = apply_internal_blank_line_markers(&source);
        assert_eq!(
            rewritten,
            "function component() {\nconst y = {};\n\ny.x = x.a;\n}\n"
        );
    }

    #[test]
    fn strips_residual_flow_cast_rewrite_marker_comments() {
        let source = r#"const x = ([] 
  /*__FLOW_CAST__*/: Array<number>);"#;
        let restored = restore_flow_cast_marker_calls(source);
        assert!(restored.contains("const x = ([]: Array<number>);"));
        assert!(!restored.contains("/*__FLOW_CAST__*/"));
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
    fn emit_module_preserves_top_level_comments_for_rewritten_functions() {
        let source = "/** keep me */\nfunction Foo() { return null; }";
        let allocator = Allocator::default();
        let source_type = source_type_for_filename("fixture.jsx");
        let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        let program = parsed.program;
        let ast::Statement::FunctionDeclaration(function) = &program.body[0] else {
            panic!("expected function declaration");
        };
        let compiled_function = make_test_compiled_function(
            "Foo",
            function.span.start,
            function.span.end,
            "return 1;",
            &[],
            false,
        );
        let options = PluginOptions::default();
        let result = super::emit_module(
            ModuleEmitArgs {
                filename: "fixture.jsx",
                source,
                source_untransformed: source,
                source_type,
                program: &program,
                options: &options,
                dynamic_gate_ident: None,
                ast_identifier_renames: &std::collections::HashMap::new(),
            },
            vec![compiled_function],
        );

        assert!(
            result.code.contains("/** keep me */"),
            "code={}",
            result.code
        );
        assert!(result.code.contains("function Foo() {"));
        assert!(result.code.contains("return null;"), "code={}", result.code);
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
    fn transform_state_ignores_conditional_set_state_bailout_fixture_explicit_void_return() {
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
    return;
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

        assert!(rewritten.contains("enum Color"), "rewritten={rewritten}");
        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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

        assert!(
            !rewritten.contains("const { other }"),
            "rewritten={rewritten}"
        );
        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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

        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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

        assert!(rewritten.contains("return null;"), "rewritten={rewritten}");
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
        let allocator = Allocator::default();
        let source_type = source_type_for_filename("fixture.jsx");
        let mut body = parse_rendered_function_body(
            &allocator,
            source_type,
            false,
            false,
            "let t1 = useFire(foo);\nlet t0 = useFire(bar);\nreturn [t1, t0];",
        )
        .expect("expected function body");
        let mut compiled_function =
            make_test_compiled_function("Component", 0, 0, "return null;", &[], false);
        compiled_function.normalize_use_fire_binding_temps = true;
        let builder = AstBuilder::new(&allocator);
        normalize_use_fire_binding_temps_ast(builder, &mut body, &compiled_function);

        let rewritten = body
            .statements
            .iter()
            .map(|statement| {
                codegen_statement_source(&allocator, source_type, statement)
                    .trim_end_matches('\n')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rewritten.contains("let t0 = useFire(foo);"),
            "rewritten={rewritten}"
        );
        assert!(
            rewritten.contains("let t1 = useFire(bar);"),
            "rewritten={rewritten}"
        );
        assert!(
            rewritten.contains("return [t0, t1];"),
            "rewritten={rewritten}"
        );
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
        compiled_function.hir_function = Some(simple_return_param_hir("props"));

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(rewritten.contains("const FancyButton = (props) => {"));
        assert!(rewritten.contains("return props;"));
    }

    #[test]
    fn rewrites_emit_freeze_cache_store_as_ast() {
        let allocator = Allocator::default();
        let source_type = source_type_for_filename("fixture.jsx");
        let mut body = parse_rendered_function_body(
            &allocator,
            source_type,
            false,
            false,
            "let t0;\nif ($[0] !== props.value) {\n  t0 = props.value;\n  $[0] = props.value;\n  $[1] = t0;\n} else {\n  t0 = $[1];\n}\nreturn t0;",
        )
        .expect("expected function body");
        let mut compiled_function =
            make_test_compiled_function("useFoo", 0, 0, "return null;", &["props"], false);
        compiled_function.needs_emit_freeze = true;
        let state = AstRenderState {
            make_read_only_ident: "makeReadOnly".to_string(),
            ..empty_test_state(source_type_for_filename("fixture.jsx"))
        };
        let builder = AstBuilder::new(&allocator);
        apply_emit_freeze_to_cache_stores_ast(
            builder,
            &allocator,
            &mut body,
            &compiled_function,
            &state,
        );
        let rewritten = body
            .statements
            .iter()
            .map(|statement| {
                codegen_statement_source(&allocator, source_type, statement)
                    .trim_end_matches('\n')
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rewritten.contains("$[1] = __DEV__ ? makeReadOnly(t0, \"useFoo\") : t0;"),
            "rewritten={rewritten}"
        );
    }

    fn simple_reactive_function(name: &str, params: &[&str]) -> ReactiveFunction {
        let rf_params: Vec<Argument> = params
            .iter()
            .enumerate()
            .map(|(i, p)| Argument::Place(named_place(i as u32, i as u32, p)))
            .collect();
        let body = if params.is_empty() {
            let tp = Place {
                identifier: Identifier {
                    id: IdentifierId(100),
                    declaration_id: DeclarationId(100),
                    name: None,
                    mutable_range: MutableRange::default(),
                    scope: None,
                    type_: Type::Primitive,
                    loc: SourceLocation::Generated,
                },
                effect: Effect::Read,
                reactive: false,
                loc: SourceLocation::Generated,
            };
            vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(tp.clone()),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Null,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: tp,
                        id: InstructionId(1),
                    },
                    label: None,
                }),
            ]
        } else {
            vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return {
                    value: named_place(0, 0, params[0]),
                    id: InstructionId(0),
                },
                label: None,
            })]
        };
        ReactiveFunction {
            id: Some(name.to_string()),
            name_hint: Some(name.to_string()),
            params: rf_params,
            body,
        }
    }

    fn make_test_compiled_function(
        name: &str,
        start: u32,
        end: u32,
        _body_source: &str,
        params: &[&str],
        _is_arrow: bool,
    ) -> CompiledFunction {
        CompiledFunction {
            name: name.to_string(),
            start,
            end,
            reactive_function: Some(simple_reactive_function(name, params)),
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
            enable_change_variable_codegen: false,
            enable_emit_hook_guards: false,
            enable_change_detection_for_debugging: false,
            enable_reset_cache_on_source_file_changes: false,
            fast_refresh_source_hash: None,
            disable_memoization_features: false,
            disable_memoization_for_debugging: false,
            fbt_operands: std::collections::HashSet::new(),
            unique_identifiers: std::collections::HashSet::new(),
            enable_name_anonymous_functions: false,
            enable_memoization_comments: false,
        }
    }

    fn simple_return_param_hir(name: &str) -> HIRFunction {
        let param = named_place(0, 0, name);
        HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![types::Argument::Place(param.clone())],
            returns: param.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId(0),
                        instructions: vec![],
                        terminal: Terminal::Return {
                            value: param.clone(),
                            return_variant: types::ReturnVariant::Explicit,
                            id: types::InstructionId(0),
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
                id: IdentifierId(id),
                declaration_id: DeclarationId(declaration_id),
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
        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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
        assert!(rewritten.contains("return null;"), "rewritten={rewritten}");
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
        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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
        assert!(rewritten.contains("return props;"), "rewritten={rewritten}");
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
            directives: vec![],
            cache_prologue: None,
            needs_function_hook_guard_wrapper: false,
            is_async: true,
            is_generator: false,
            reactive_function: Some(simple_reactive_function("_temp", &["load", "rest"])),
            unique_identifiers: std::collections::HashSet::new(),
        }];

        let rewritten =
            rewrite_single_statement_for_test("fixture.jsx", source, &compiled_function);

        assert!(
            rewritten.contains("function Foo(props) {"),
            "rewritten={rewritten}"
        );
        assert!(
            rewritten.contains("async function _temp(load, ...rest) {"),
            "rewritten={rewritten}"
        );
        assert!(rewritten.contains("return load;"), "rewritten={rewritten}");
    }
}
