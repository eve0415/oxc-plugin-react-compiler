use crate::CompileResult;

use super::{CompiledFunction, ModuleEmitArgs};

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    let ModuleEmitArgs {
        filename,
        source,
        source_untransformed,
        program,
        options,
        dynamic_gate_ident,
    } = args;

    let needs_cache_import = compiled.iter().any(|c| c.needs_cache_import);
    let needs_fire_import = compiled.iter().any(|c| c.has_fire_rewrite);
    let top_level_bindings = crate::pipeline::collect_top_level_bindings(program);
    let mut output = String::new();

    let is_script = source.contains("// @script") || source.contains("/* @script");

    let mut all_bindings = crate::pipeline::collect_all_program_bindings(program);
    let mut cache_import_name = crate::pipeline::generate_unique_name("_c", &all_bindings);
    let runtime_import_merge_plan = if !is_script && (needs_cache_import || needs_fire_import) {
        crate::pipeline::plan_runtime_import_merge(
            program,
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
        options.environment.enable_emit_freeze && compiled.iter().any(|c| c.needs_emit_freeze);
    let mut make_read_only_ident = String::new();
    if needs_freeze_import {
        make_read_only_ident =
            crate::pipeline::generate_unique_import_binding("makeReadOnly", &all_bindings);
        all_bindings.insert(make_read_only_ident.clone());
    }

    let needs_instrument_import = options.environment.enable_emit_instrument_forget
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
    let needs_hook_guard_import =
        options.environment.enable_emit_hook_guards && compiled.iter().any(|c| c.needs_hook_guards);
    let mut hook_guard_ident = String::new();
    if needs_hook_guard_import {
        hook_guard_ident =
            crate::pipeline::generate_unique_import_binding("$dispatcherGuard", &all_bindings);
        all_bindings.insert(hook_guard_ident.clone());
    }
    let needs_structural_check_import = options.environment.enable_change_detection_for_debugging
        && compiled.iter().any(|c| c.needs_structural_check_import);
    let mut structural_check_ident = String::new();
    if needs_structural_check_import {
        structural_check_ident =
            crate::pipeline::generate_unique_import_binding("$structuralCheck", &all_bindings);
        all_bindings.insert(structural_check_ident.clone());
    }
    let lower_context_access_config = options.environment.lower_context_access.as_ref();
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
        filename
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(filename)
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
                "import {{ c as {}, useFire }} from \"react/compiler-runtime\"",
                cache_import_name
            ),
            (true, false) => format!(
                "import {{ c as {} }} from \"react/compiler-runtime\"",
                cache_import_name
            ),
            (false, true) => "import { useFire } from \"react/compiler-runtime\"".to_string(),
            (false, false) => String::new(),
        }
    };
    let mut gating_local_name: Option<String> = None;
    let mut imports_to_insert: Vec<String> = Vec::new();
    let mut runtime_import_index: Option<usize> = None;
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
                "const {{ {} }} = require(\"react-compiler-runtime\")",
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
            format!("import {{ {} }} from \"react-compiler-runtime\"", specs)
        };
        imports_to_insert.push(support_import);
    }
    let runtime_import_covered_by_existing =
        runtime_import_merge_plan.as_ref().is_some_and(|plan| {
            (!needs_cache_import || plan.has_cache_after)
                && (!needs_fire_import || plan.has_use_fire_after)
        });
    if (needs_cache_import || needs_fire_import) && !runtime_import_covered_by_existing {
        runtime_import_index = Some(imports_to_insert.len());
        imports_to_insert.push(runtime_import);
    }
    if needs_lower_context_access_import
        && !lower_context_access_module.is_empty()
        && lower_context_access_module != "react-compiler-runtime"
    {
        let lower_context_import = if is_script {
            if lower_context_access_imported == lower_context_access_ident {
                format!(
                    "const {{ {} }} = require(\"{}\")",
                    lower_context_access_imported, lower_context_access_module
                )
            } else {
                format!(
                    "const {{ {}: {} }} = require(\"{}\")",
                    lower_context_access_imported,
                    lower_context_access_ident,
                    lower_context_access_module
                )
            }
        } else if lower_context_access_imported == lower_context_access_ident {
            format!(
                "import {{ {} }} from \"{}\"",
                lower_context_access_imported, lower_context_access_module
            )
        } else {
            format!(
                "import {{ {} as {} }} from \"{}\"",
                lower_context_access_imported,
                lower_context_access_ident,
                lower_context_access_module
            )
        };
        imports_to_insert.push(lower_context_import);
    }
    if let Some((source_mod, base)) = options
        .gating
        .as_ref()
        .map(|g| (g.source.as_str(), g.import_specifier_name.as_str()))
        .or_else(|| {
            dynamic_gate_ident
                .zip(options.dynamic_gating.as_ref().map(|g| g.source.as_str()))
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
        let gate_import = if is_script {
            if local == base {
                format!("const {{ {} }} = require(\"{}\")", base, source_mod)
            } else {
                format!(
                    "const {{ {}: {} }} = require(\"{}\")",
                    base, local, source_mod
                )
            }
        } else if local == base {
            format!("import {{ {} }} from \"{}\"", base, source_mod)
        } else {
            format!("import {{ {} as {} }} from \"{}\"", base, local, source_mod)
        };
        if needs_cache_import {
            imports_to_insert.push(gate_import);
        }
    }

    let mut source_skip = 0usize;
    if !imports_to_insert.is_empty() {
        let mut leading_comment: Option<String> = None;
        let trimmed = source.trim_start();
        if trimmed.starts_with("//") {
            if let Some(nl) = trimmed.find('\n') {
                let comment = trimmed[..nl].trim();
                let comment_start = source.len() - trimmed.len();
                if comment.starts_with("// @flow") {
                    source_skip = comment_start + nl + 1;
                } else {
                    leading_comment = Some(comment.to_string());
                    source_skip = comment_start + nl + 1;
                }
            } else {
                leading_comment = Some(trimmed.trim().to_string());
            }
        } else if trimmed.starts_with("/*")
            && let Some(end_idx) = trimmed.find("*/")
        {
            let comment = &trimmed[..end_idx + 2];
            leading_comment = Some(comment.to_string());
            let comment_start = source.len() - trimmed.len();
            source_skip = comment_start + end_idx + 2;
            if source_skip < source.len() && source.as_bytes()[source_skip] == b'\n' {
                source_skip += 1;
            }
        }

        let comment_target = if gating_local_name.is_some() {
            imports_to_insert.len().saturating_sub(1)
        } else {
            runtime_import_index.unwrap_or(0)
        };
        for (idx, import_str) in imports_to_insert.iter().enumerate() {
            if let Some(comment) = leading_comment.as_ref().filter(|_| idx == comment_target) {
                output.push_str(&format!("{}; {}\n", import_str, comment));
            } else {
                output.push_str(&format!("{};\n", import_str));
            }
        }
    }

    let mut last_end = source_skip as u32;
    let mut compiled_sorted = compiled;
    compiled_sorted.sort_by_key(|c| c.start);

    for cf in &compiled_sorted {
        let before_rewritten = crate::pipeline::rewrite_source_segment_with_runtime_import_merge(
            source,
            last_end as usize,
            cf.start as usize,
            runtime_import_merge_plan.as_ref(),
        );

        let mut body = cf.generated_body.clone();
        if cache_import_name != "_c" {
            body = body.replacen("_c(", &format!("{}(", cache_import_name), 1);
        }
        if !cf.param_destructurings.is_empty() && !body.contains("=== undefined ?") {
            let pruned: Vec<String> = cf
                .param_destructurings
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let after: String = cf.param_destructurings[i + 1..].join("\n");
                    let context = format!("{}\n{}", body, after);
                    crate::pipeline::prune_unused_destructuring(d, &context)
                })
                .collect();
            body = crate::pipeline::insert_param_destructurings(&body, &pruned);
        }
        if !cf.preserved_body_statements.is_empty() {
            body = crate::pipeline::insert_preserved_body_statements(
                &body,
                &cf.preserved_body_statements,
            );
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
                should_instrument_ident,
                use_render_counter_ident,
                rendered_name,
                instrument_source_path
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
                &make_read_only_ident,
                freeze_name,
            );
        }
        if cf.needs_hook_guards {
            body = crate::pipeline::maybe_align_hook_guard_name(&body, &hook_guard_ident);
        }
        if cf.needs_structural_check_import {
            body =
                crate::pipeline::maybe_align_structural_check_name(&body, &structural_check_ident);
        }
        if cf.needs_lower_context_access && !lower_context_access_ident.is_empty() {
            body = crate::pipeline::maybe_align_lower_context_access_name(
                &body,
                &lower_context_access_imported,
                &lower_context_access_ident,
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
        let preserve_original = should_preserve_original_layout_for_equivalent_output(
            cf,
            &compiled_fn_src,
            &original_src_raw,
        );
        let mut before_emit = before_rewritten;
        let mut replacement_src = if preserve_original {
            original_src_raw.clone()
        } else {
            compiled_fn_src.clone()
        };
        let mut next_source_start = cf.end;
        if let Some(gate_name) = gating_local_name.as_ref().filter(|_| cf.needs_cache_import) {
            let gate_call = format!("{}()", gate_name);
            let has_parenthesized_arrow_body = cf.is_arrow && original_src_raw.contains("=> (");
            let original_src = gated_uncompiled_function_source(source, cf);
            let before_trimmed_owned = before_emit.trim_end().to_string();
            let before_trimmed = before_trimmed_owned.as_str();
            let export_default_ctx = before_trimmed.ends_with("export default");
            let expression_ctx =
                cf.is_arrow || crate::pipeline::is_expression_context(before_trimmed);
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
                    crate::pipeline::has_early_binding_reference(&before_emit, &cf.name);
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
        output.push_str(&before_emit);
        output.push_str(&replacement_src);

        if !cf.outlined_functions.is_empty() {
            for (fn_name, fn_params, fn_body) in &cf.outlined_functions {
                let trimmed = fn_body.trim();
                if trimmed.is_empty() {
                    output.push_str(&format!("\nfunction {}({}) {{}}", fn_name, fn_params));
                } else {
                    output.push_str(&format!(
                        "\nfunction {}({}) {{\n{}\n}}",
                        fn_name, fn_params, trimmed
                    ));
                }
            }
        }

        last_end = next_source_start;
    }

    if (last_end as usize) < source.len() {
        output.push_str(
            &crate::pipeline::rewrite_source_segment_with_runtime_import_merge(
                source,
                last_end as usize,
                source.len(),
                runtime_import_merge_plan.as_ref(),
            ),
        );
    }

    if let Some(gate_name) = gating_local_name.as_ref().filter(|_| needs_cache_import) {
        output = crate::pipeline::gate_fixture_entrypoint_arrows(output, gate_name);
    }

    let transformed =
        normalize_for_transform_flag(&output) != normalize_for_transform_flag(source_untransformed);
    CompileResult {
        transformed,
        code: output,
        map: None,
    }
}

pub(crate) fn gated_uncompiled_function_source(source: &str, cf: &CompiledFunction) -> String {
    let original_src = source[cf.start as usize..cf.end as usize]
        .trim()
        .to_string();
    if !cf.is_function_declaration {
        return original_src;
    }

    let body_start = cf.body_start as usize;
    let body_end = cf.body_end as usize;
    let Some(body_src) = source.get(body_start..body_end).map(str::trim) else {
        return original_src;
    };
    if body_src.is_empty() {
        return original_src;
    }

    let async_prefix = if cf.is_async { "async " } else { "" };
    let gen_prefix = if cf.is_generator { "*" } else { "" };
    if cf.name.is_empty() {
        return original_src;
    }

    format!(
        "{}function {}{}({}) {}",
        async_prefix, gen_prefix, cf.name, cf.original_params_str, body_src
    )
}

fn non_empty_trimmed_lines(src: &str) -> Vec<&str> {
    src.lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

pub(crate) fn should_preserve_original_layout_for_equivalent_output(
    cf: &CompiledFunction,
    compiled_src: &str,
    original_src: &str,
) -> bool {
    if cf.needs_cache_import
        || cf.needs_instrument_forget
        || cf.needs_emit_freeze
        || !cf.outlined_functions.is_empty()
        || cf.has_fire_rewrite
        || cf.needs_hook_guards
        || cf.needs_structural_check_import
        || cf.needs_lower_context_access
    {
        return false;
    }
    non_empty_trimmed_lines(compiled_src) == non_empty_trimmed_lines(original_src)
}

pub(crate) fn normalize_for_transform_flag(code: &str) -> String {
    let compact: String = code
        .replace("\r\n", "\n")
        .trim_end()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let normalized_quotes = compact.replace('\'', "\"");
    let normalized_arrows =
        crate::pipeline::strip_single_param_arrow_parens_for_transform_flag(&normalized_quotes);
    crate::pipeline::strip_trailing_commas_before_closer_for_transform_flag(&normalized_arrows)
}
