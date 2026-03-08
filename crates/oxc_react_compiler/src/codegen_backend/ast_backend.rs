use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};

use crate::CompileResult;

use super::{CompiledBodyPayload, CompiledFunction, ModuleEmitArgs};

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
    imports_to_insert: Vec<String>,
    runtime_import_merge_plan: Option<crate::pipeline::RuntimeImportMergePlan>,
    instrument_source_path: String,
}

struct RenderedCompiledFunction {
    before_emit: String,
    replacement_src: String,
    next_source_start: u32,
    outlined_functions: Vec<RenderedOutlinedFunction>,
}

struct RenderedOutlinedFunction {
    source: String,
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
    for import_src in &state.imports_to_insert {
        body.extend(parse_statements(
            &allocator,
            state.source_type,
            allocator.alloc_str(import_src),
        )?);
        rendered_prefix.push_str(import_src);
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
            if let Some(replacement) = plan.replacement.as_deref() {
                body.extend(parse_statements(
                    &allocator,
                    state.source_type,
                    allocator.alloc_str(replacement),
                )?);
                rendered_prefix.push_str(replacement);
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
            && let Some(statement) = try_lower_compiled_statement_ast(builder, stmt_compiled[0])
        {
            let statement_source = codegen_statement_source(&allocator, state.source_type, &statement);
            body.push(statement);
            rendered_prefix.push_str(statement_source.trim_end_matches('\n'));
            rendered_prefix.push('\n');
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
                body.push(statement);
            } else {
                body.extend(parse_statements(
                    &allocator,
                    state.source_type,
                    allocator.alloc_str(&outlined.source),
                )?);
            }
            rendered_prefix.push_str(&outlined.source);
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

    let runtime_import = if is_script {
        match (needs_cache_import, needs_fire_import) {
            (true, true) => format!(
                "const {{ c: {}, useFire }} = require(\"react/compiler-runtime\")",
                cache_import_name
            ),
            (true, false) => format!(
                "const {{ c: {} }} = require(\"react/compiler-runtime\")",
                cache_import_name
            ),
            (false, true) => "const { useFire } = require(\"react/compiler-runtime\")".to_string(),
            (false, false) => String::new(),
        }
    } else {
        match (needs_cache_import, needs_fire_import) {
            (true, true) => format!(
                "import {{ c as {}, useFire }} from \"react/compiler-runtime\";",
                cache_import_name
            ),
            (true, false) => format!(
                "import {{ c as {} }} from \"react/compiler-runtime\";",
                cache_import_name
            ),
            (false, true) => "import { useFire } from \"react/compiler-runtime\";".to_string(),
            (false, false) => String::new(),
        }
    };

    let mut gating_local_name = None;
    let mut imports_to_insert = Vec::new();
    let mut runtime_support_specs: Vec<(&str, &str)> = Vec::new();
    if needs_freeze_import {
        runtime_support_specs.push(("makeReadOnly", &make_read_only_ident));
    }
    if needs_instrument_import {
        runtime_support_specs.push(("shouldInstrument", &should_instrument_ident));
        runtime_support_specs.push(("useRenderCounter", &use_render_counter_ident));
    }
    if needs_hook_guard_import {
        runtime_support_specs.push(("$dispatcherGuard", &hook_guard_ident));
    }
    if needs_structural_check_import {
        runtime_support_specs.push(("$structuralCheck", &structural_check_ident));
    }
    if needs_lower_context_access_import && lower_context_access_module == "react-compiler-runtime"
    {
        runtime_support_specs.push((&lower_context_access_imported, &lower_context_access_ident));
    }
    if !runtime_support_specs.is_empty() {
        let support_import = if is_script {
            let specs = runtime_support_specs
                .iter()
                .map(|(imported, local)| {
                    if imported == local {
                        (*imported).to_string()
                    } else {
                        format!("{}: {}", imported, local)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "const {{ {} }} = require(\"react-compiler-runtime\");",
                specs
            )
        } else {
            let specs = runtime_support_specs
                .iter()
                .map(|(imported, local)| {
                    if imported == local {
                        (*imported).to_string()
                    } else {
                        format!("{} as {}", imported, local)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("import {{ {} }} from \"react-compiler-runtime\";", specs)
        };
        imports_to_insert.push(support_import);
    }

    let runtime_import_covered_by_existing =
        runtime_import_merge_plan.as_ref().is_some_and(|plan| {
            (!needs_cache_import || plan.has_cache_after)
                && (!needs_fire_import || plan.has_use_fire_after)
        });
    if (needs_cache_import || needs_fire_import) && !runtime_import_covered_by_existing {
        imports_to_insert.push(runtime_import);
    }

    if needs_lower_context_access_import
        && !lower_context_access_module.is_empty()
        && lower_context_access_module != "react-compiler-runtime"
    {
        let lower_context_import = if is_script {
            if lower_context_access_imported == lower_context_access_ident {
                format!(
                    "const {{ {} }} = require(\"{}\");",
                    lower_context_access_imported, lower_context_access_module
                )
            } else {
                format!(
                    "const {{ {}: {} }} = require(\"{}\");",
                    lower_context_access_imported,
                    lower_context_access_ident,
                    lower_context_access_module
                )
            }
        } else if lower_context_access_imported == lower_context_access_ident {
            format!(
                "import {{ {} }} from \"{}\";",
                lower_context_access_imported, lower_context_access_module
            )
        } else {
            format!(
                "import {{ {} as {} }} from \"{}\";",
                lower_context_access_imported,
                lower_context_access_ident,
                lower_context_access_module
            )
        };
        imports_to_insert.push(lower_context_import);
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
            let gate_import = if is_script {
                if local == base {
                    format!("const {{ {} }} = require(\"{}\");", base, source_mod)
                } else {
                    format!(
                        "const {{ {}: {} }} = require(\"{}\");",
                        base, local, source_mod
                    )
                }
            } else if local == base {
                format!("import {{ {} }} from \"{}\";", base, source_mod)
            } else {
                format!(
                    "import {{ {} as {} }} from \"{}\";",
                    base, local, source_mod
                )
            };
            imports_to_insert.push(gate_import);
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

fn render_compiled_function(
    cf: &CompiledFunction,
    mut before_emit: String,
    context_before: &str,
    source: &str,
    state: &AstRenderState,
) -> RenderedCompiledFunction {
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

    let outlined_functions = cf
        .outlined_functions
        .iter()
        .map(|(fn_name, fn_params, fn_body)| {
            let hir_function = cf
                .hir_outlined_functions
                .iter()
                .find(|(outlined_name, _)| outlined_name == fn_name)
                .map(|(_, hir_function)| hir_function.clone());
            let body_src = fn_body.as_str();
            let trimmed = body_src.trim();
            let source = if trimmed.is_empty() {
                format!("function {}({}) {{}}", fn_name, fn_params)
            } else {
                format!("function {}({}) {{\n{}\n}}", fn_name, fn_params, trimmed)
            };
            RenderedOutlinedFunction {
                source,
                hir_function,
            }
        })
        .collect();

    RenderedCompiledFunction {
        before_emit,
        replacement_src,
        next_source_start,
        outlined_functions,
    }
}

fn try_lower_compiled_statement_ast<'a>(
    builder: AstBuilder<'a>,
    cf: &CompiledFunction,
) -> Option<ast::Statement<'a>> {
    if cf.body_payload != CompiledBodyPayload::LowerFromFinalHir
        || !cf.is_function_declaration
        || cf.needs_cache_import
        || !cf.param_destructurings.is_empty()
        || !cf.preserved_body_statements.is_empty()
        || cf.needs_instrument_forget
        || cf.needs_emit_freeze
        || cf.needs_hook_guards
        || cf.needs_structural_check_import
        || cf.needs_lower_context_access
    {
        return None;
    }
    super::hir_to_ast::try_lower_function_declaration_ast(builder, cf.hir_function.as_ref()?)
}

fn maybe_gate_entrypoint_source(source: String, gate_name: &str) -> String {
    crate::pipeline::gate_fixture_entrypoint_arrows(source, gate_name)
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

    use super::{maybe_gate_entrypoint_source, parse_statements, source_type_for_filename};

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
}
