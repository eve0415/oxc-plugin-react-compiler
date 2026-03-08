use super::CompiledFunction;

pub(crate) fn gated_uncompiled_function_source(source: &str, cf: &CompiledFunction) -> String {
    let original_src = source[cf.start as usize..cf.end as usize].trim().to_string();
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
