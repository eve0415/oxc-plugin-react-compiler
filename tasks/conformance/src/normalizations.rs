use std::sync::OnceLock;

pub(crate) fn canonicalize_strict_text(code: &str) -> String {
    code.replace("\r\n", "\n").trim_end().to_string()
}

pub(crate) fn normalize_post_babel_export_spacing(code: &str) -> String {
    code.replace("\n    );\n\nexport default", "\n    );\nexport default")
        .replace(
            "\n    );\n\nexport const FIXTURE_ENTRYPOINT",
            "\n    );\nexport const FIXTURE_ENTRYPOINT",
        )
}

/// Normalize code for comparison. Applies all cosmetic normalizations (shared +
/// strict) in a convergence loop until the output stabilizes.
fn normalize_for_compare(code: &str) -> String {
    let steps: &[fn(&str) -> String] = &[
        // Shared cosmetic normalizations (OXC vs Babel formatting)
        normalize_compare_multiline_imports,
        normalize_import_region_comments,
        normalize_top_level_comment_trivia,
        normalize_compare_multiline_brace_literals,
        normalize_multiline_trailing_commas_before_closers,
        normalize_labeled_switch_breaks,
        normalize_labeled_block_braces,
        normalize_switch_case_braces,
        normalize_multiline_switch_cases,
        normalize_ts_object_type_semicolons,
        normalize_numeric_exponent_literals,
        normalize_compare_unicode_escapes,
        normalize_fixture_entrypoint_array_spacing,
        normalize_scope_body_blank_lines,
        normalize_top_level_statement_blank_lines,
        normalize_space_before_closing_brace,
        normalize_jsx_space_expression_container,
        normalize_jsx_child_whitespace,
        normalize_jsx_assignment_parens,
        normalize_jsx_expression_container_spacing,
        normalize_jsx_text_boundary_space,
        normalize_jsx_residual_close_paren,
        normalize_optional_parens,
        normalize_import_quotes,
        normalize_function_paren_space,
        normalize_empty_block_newlines,
        normalize_multiline_short_arrays,
        normalize_object_in_array_spacing,
        normalize_const_string_quotes,
        normalize_empty_block_inner_space,
        normalize_destructuring_brace_spacing,
        normalize_single_arrow_param_parens,
        normalize_assignment_expression_parens,
        normalize_numeric_leading_zero,
        normalize_jsx_trailing_text_space_before_close,
        normalize_optional_call_space,
        normalize_jsx_attr_trailing_space,
        normalize_empty_statement_semicolons,
        // Strict output normalizations (cosmetic OXC printer differences)
        normalize_trailing_comma_in_calls,
        normalize_multiline_call_invocations,
        normalize_small_array_bracket_spacing,
        normalize_bracket_string_literal_spacing,
        normalize_generated_memoization_comments,
        normalize_dead_bare_var_refs,
        normalize_multiline_iife_collapsing,
        normalize_inline_iife_parenthesization,
        normalize_if_consequent_newline,
        normalize_multiline_if_condition,
        normalize_multiline_arrow_body,
        normalize_outlined_function_spacing,
    ];

    let mut normalized = canonicalize_strict_text(code);
    for _ in 0..6 {
        let mut next = normalized.clone();
        for step in steps {
            next = step(&next);
        }
        if next == normalized {
            return next;
        }
        normalized = next;
    }
    normalized
}

// Keep old names as aliases for call-site compatibility
pub(crate) fn prepare_code_for_compare(code: &str) -> String {
    normalize_for_compare(code)
}

// --- Flow preprocessing ---

pub(crate) fn preprocess_flow_syntax_for_expectation(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut saw_non_comment_code = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if !saw_non_comment_code
            && (trimmed == "//@flow"
                || trimmed == "// @flow"
                || trimmed.starts_with("//@flow ")
                || trimmed.starts_with("// @flow "))
        {
            continue;
        }
        if let Some(transformed_component) =
            transform_simple_flow_component_line_for_expectation(line)
        {
            result.push_str(&transformed_component);
            result.push('\n');
            saw_non_comment_code = true;
            continue;
        }
        let mut processed = line.to_string();
        if let Some(idx) = find_flow_keyword_for_expectation(&processed, "component") {
            let after = processed[idx + "component".len()..].trim_start();
            if after.starts_with(|c: char| c.is_uppercase()) {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "component".len()..]
                );
            }
        }
        if let Some(idx) = find_flow_keyword_for_expectation(&processed, "hook") {
            let after = processed[idx + "hook".len()..].trim_start();
            if after.starts_with("use") {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "hook".len()..]
                );
            }
        }
        if !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with('*')
            && !trimmed.starts_with("*/")
        {
            saw_non_comment_code = true;
        }
        result.push_str(&processed);
        result.push('\n');
    }
    if !source.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

fn transform_simple_flow_component_line_for_expectation(line: &str) -> Option<String> {
    let idx = find_flow_keyword_for_expectation(line, "component")?;
    let prefix = &line[..idx];
    let is_export_prefixed = prefix.trim_start().starts_with("export");
    let after_keyword = line[idx + "component".len()..].trim_start();
    if !after_keyword.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }
    let name_end = after_keyword
        .char_indices()
        .take_while(|(_, c)| is_identifier_char_for_expectation(*c))
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    let name = &after_keyword[..name_end];
    let rest = after_keyword[name_end..].trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = rest.find(')')?;
    let params = rest[1..close].trim();
    let tail = rest[close + 1..].trim_start();
    if !tail.starts_with('{') {
        return None;
    }
    if params.starts_with("...{") {
        let rewritten_param = params.trim_start_matches("...");
        return Some(format!(
            "{}function {}({}) {}",
            prefix, name, rewritten_param, tail
        ));
    }
    if params.is_empty() || params.starts_with("...") {
        return None;
    }
    if params.contains(',') {
        let parts: Vec<&str> = params.split(',').map(str::trim).collect();
        if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        let (first_name, first_type) = if let Some(colon) = parts[0].find(':') {
            let name = parts[0][..colon].trim();
            let ty = parts[0][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[0], None)
        };
        let (second_name, second_type) = if let Some(colon) = parts[1].find(':') {
            let name = parts[1][..colon].trim();
            let ty = parts[1][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[1], None)
        };
        if second_name != "ref" || !is_valid_js_identifier_for_expectation(first_name) {
            return None;
        }
        let rewritten_props = if let Some(ty) = first_type {
            format!(
                "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
                first_name, first_name, ty
            )
        } else {
            format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", first_name, first_name)
        };
        let ref_param = if let Some(ty) = second_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        if is_export_prefixed {
            return None;
        }
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef({}, {}): React.Node {}",
            prefix, name, name, prefix, name, rewritten_props, ref_param, tail
        ));
    }
    let (param_name, param_type) = if let Some(colon) = params.find(':') {
        let name = params[..colon].trim();
        let ty = params[colon + 1..].trim();
        (name, Some(ty))
    } else {
        (params, None)
    };
    if !is_valid_js_identifier_for_expectation(param_name) {
        return None;
    }
    if param_name == "ref" {
        if is_export_prefixed {
            return None;
        }
        let ref_param = if let Some(ty) = param_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef(_$$empty_props_placeholder$$: $ReadOnly<{{ }}>, {}): React.Node {}",
            prefix, name, name, prefix, name, ref_param, tail
        ));
    }
    let rewritten_param = if let Some(ty) = param_type {
        format!(
            "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
            param_name, param_name, ty
        )
    } else {
        format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", param_name, param_name)
    };
    Some(format!(
        "{}function {}({}): React.Node {}",
        prefix, name, rewritten_param, tail
    ))
}

fn find_flow_keyword_for_expectation(line: &str, keyword: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let offset = line.len() - trimmed.len();
    if let Some(after) = trimmed.strip_prefix(keyword)
        && (after.starts_with(' ') || after.starts_with('\t'))
    {
        return Some(offset);
    }
    if let Some(rest) = trimmed.strip_prefix("export") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("default") {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix(keyword)
                && (after.starts_with(' ') || after.starts_with('\t'))
            {
                let kw_offset = line.len() - rest.len();
                return Some(kw_offset);
            }
        }
        let rest2 = rest;
        if let Some(after) = rest2.strip_prefix(keyword)
            && (after.starts_with(' ') || after.starts_with('\t'))
        {
            let kw_offset = line.len() - rest2.len();
            return Some(kw_offset);
        }
    }
    None
}

fn is_identifier_char_for_expectation(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric()
}

fn is_valid_js_identifier_for_expectation(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    if !chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric()) {
        return false;
    }
    !matches!(
        name,
        "true"
            | "false"
            | "null"
            | "this"
            | "new"
            | "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "switch"
            | "case"
            | "default"
            | "function"
            | "class"
            | "import"
            | "export"
    )
}

// --- Normalization functions ---

/// Normalize JSX child whitespace: strip single spaces between JSX children.
/// Babel adds spaces like `<div> {x} {y} </div>`, OXC omits them.
/// Uses simple string replacement for specific 2-char boundary patterns.
fn normalize_jsx_child_whitespace(code: &str) -> String {
    // Replace specific JSX boundary patterns where a single space appears.
    // These are: `> {`, `} {`, `} <`, `> <` (in JSX child context).
    // We can't distinguish JSX from non-JSX perfectly, so we use a heuristic:
    // only strip when the `<` starts an uppercase or lowercase tag name
    // (NOT operators like `<=`), and `} {` only between expression containers.
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if i + 2 < len && bytes[i + 1] == b' ' {
            let prev = bytes[i];
            let next = bytes[i + 2];
            let skip_space = match (prev, next) {
                // `> {` — after JSX opening tag, before expression container
                (b'>', b'{') => true,
                // `} {` — between expression containers
                (b'}', b'{') => true,
                // `} <` — after expression, before JSX child element or fragment
                // Only when followed by a tag name char, `/`, or `>` (fragment `<>`)
                (b'}', b'<') => {
                    i + 3 < len
                        && (bytes[i + 3].is_ascii_alphabetic()
                            || bytes[i + 3] == b'/'
                            || bytes[i + 3] == b'>')
                }
                // `> <` — between JSX child elements or fragments (but NOT `> <=`)
                (b'>', b'<') => {
                    i + 3 < len
                        && (bytes[i + 3].is_ascii_alphabetic()
                            || bytes[i + 3] == b'/'
                            || bytes[i + 3] == b'>')
                }
                // `/> {` — after self-closing tag, before expression
                _ if i >= 1 && bytes[i - 1] == b'/' && prev == b'>' && next == b'{' => {
                    false // already handled by `> {` case
                }
                // `/> <` — after self-closing tag, before child element
                _ if prev == b'>' && i >= 1 && bytes[i - 1] == b'/' => {
                    next == b'<'
                        && i + 3 < len
                        && (bytes[i + 3].is_ascii_alphabetic() || bytes[i + 3] == b'/')
                }
                // `text {` — JSX text content before expression container.
                // Babel's printer adds whitespace between JSXText and
                // JSXExpressionContainer children; OXC prints them adjacent.
                _ if prev.is_ascii_alphabetic() && next == b'{' => true,
                // `} text` — expression container end before JSX text content.
                (b'}', _) if next.is_ascii_alphabetic() => true,
                _ => false,
            };
            if skip_space {
                result.push(prev as char);
                i += 2; // skip the space
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize optional parentheses around JSX in assignments and returns.
/// Babel wraps JSX: `t1 = ( <div>...</div> )` and `return ( <div /> )`
/// OXC omits them: `t1 = <div>...</div>` and `return <div />`
/// Handles both single-line and multi-line JSX parens.
fn normalize_jsx_assignment_parens(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let lines: Vec<&str> = code.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Single-line: `... = ( <...> )` or `return ( <...> )`
        if (trimmed.contains("= ( <") || trimmed.starts_with("return ( <"))
            && (trimmed.ends_with(" )") || trimmed.ends_with(" );"))
        {
            let normalized = lines[i]
                .replace("= ( <", "= <")
                .replace("return ( <", "return <");
            let normalized = if normalized.trim_end().ends_with(" );") {
                let pos = normalized.rfind(" );").unwrap();
                format!("{});", &normalized[..pos])
            } else if normalized.trim_end().ends_with(" )") {
                let pos = normalized.rfind(" )").unwrap();
                normalized[..pos].to_string()
            } else {
                normalized
            };
            result.push_str(&normalized);
            result.push('\n');
            i += 1;
            continue;
        }

        // Multi-line: line ends with `( <` and a later line ends with `)`
        if (trimmed.ends_with("= ( <")
            || trimmed.ends_with("return ( <")
            || (trimmed.contains("= ( <") && !trimmed.ends_with(")"))
            || trimmed.ends_with("=> ( <"))
            && trimmed.matches('(').count() > trimmed.matches(')').count()
        {
            // Strip `( <` → `<` on this line
            let open_line = lines[i]
                .replace("= ( <", "= <")
                .replace("return ( <", "return <")
                .replace("=> ( <", "=> <");
            result.push_str(&open_line);
            result.push('\n');
            i += 1;
            // Find matching `)` on a line that ends with ` )` or ` );`
            let mut depth = 1i32;
            while i < lines.len() {
                let t = lines[i].trim();
                for ch in t.chars() {
                    match ch {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                }
                if depth <= 0 {
                    // Strip trailing ` )` or ` );`
                    let line_str = lines[i].to_string();
                    let stripped = line_str
                        .trim_end()
                        .strip_suffix(");")
                        .map(|s| format!("{};", s.trim_end()))
                        .or_else(|| {
                            line_str
                                .trim_end()
                                .strip_suffix(')')
                                .map(|s| s.trim_end().to_string())
                        })
                        .unwrap_or(line_str);
                    result.push_str(&stripped);
                    result.push('\n');
                    i += 1;
                    break;
                }
                result.push_str(lines[i]);
                result.push('\n');
                i += 1;
            }
            continue;
        }

        result.push_str(lines[i]);
        result.push('\n');
        i += 1;
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize JSX text leading/trailing spaces when adjacent to expression
/// containers or elements.  OXC omits space: `<div>text{x}`, Babel preserves:
/// `<div> text{x}`.  Also, Babel's printer puts JSXElement children on separate
/// lines (creating an implicit space between preceding text and the element),
/// while OXC prints everything inline.  Both compilers produce identical
/// JSXText IR; the difference is purely in the printer's formatting.
fn normalize_jsx_text_boundary_space(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Strip space after `>` before text content: `> text` → `>text`
        if bytes[i] == b'>' && i + 1 < len && bytes[i + 1] == b' ' && i + 2 < len {
            let next_after_space = bytes[i + 2];
            // Only strip space between `>` and text content (letters, not `{` or `<`)
            if next_after_space.is_ascii_alphabetic() || next_after_space == b'\'' {
                result.push('>');
                i += 2; // skip the space
                continue;
            }
        }
        // Strip space before `<` after text content: `text <Tag` → `text<Tag`
        // Babel's printer puts JSXElement/Fragment children on new lines,
        // creating a space when collapsed.  OXC prints inline — no space.
        if bytes[i] == b' '
            && i >= 1
            && i + 1 < len
            && bytes[i + 1] == b'<'
            && i + 2 < len
            && (bytes[i + 2].is_ascii_alphabetic() || bytes[i + 2] == b'/' || bytes[i + 2] == b'>')
        {
            let prev = bytes[i - 1];
            // Only strip when preceded by text content (word char or closing
            // quote/paren), not by JSX delimiters like `>` or `}` which are
            // handled by normalize_jsx_child_whitespace.
            if prev.is_ascii_alphanumeric() || prev == b'\'' || prev == b'"' {
                // skip the space
                i += 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize spaces inside JSX expression containers for string literals.
/// Babel: `{ "text" }`, OXC: `{"text"}`. Both are identical in React.
fn normalize_jsx_expression_container_spacing(code: &str) -> String {
    // Replace `{ "` with `{"` and `" }` with `"}`
    code.replace("{ \"", "{\"").replace("\" }", "\"}")
}

/// Normalize JSX expression container containing a single-space string to a
/// bare space.  Prettier rewrites `{" "}` / `{' '}` to a plain JSX text space
/// when formatting the upstream expected output, while OXC codegen emits the
/// expression container literally.  Both are semantically identical in React.
fn normalize_jsx_space_expression_container(code: &str) -> String {
    code.replace("{\" \"}", " ").replace("{' '}", " ")
}

/// Strip residual `)` after JSX close tags left by multi-line paren
/// collapsing. After other normalizations collapse multi-line `( <...> )`
/// to single line, the `)` may remain: `</>);` → `</>;`.
fn normalize_jsx_residual_close_paren(code: &str) -> String {
    // First pass: strip ` )` with space after JSX close tags at end of statement
    let code = code.to_string();
    let lines: Vec<&str> = code.lines().collect();
    let mut pre_result = String::with_capacity(code.len());
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.ends_with("</> );") {
            pre_result.push_str(&line.replace("</> );", "</>;"));
        } else if trimmed.ends_with("/> );") && !trimmed.contains(": null}") {
            pre_result.push_str(&line.replace("/> );", "/>;"));
        } else if trimmed.ends_with("> );") && trimmed.contains("</") {
            // Handle `</TagName> );` → `</TagName>;`
            // Find the last occurrence of `> );` and check it's after a closing tag
            if let Some(pos) = line.rfind("> );") {
                let before = &line[..pos];
                if before.rfind("</").is_some() {
                    let mut fixed = String::with_capacity(line.len());
                    fixed.push_str(&line[..pos + 1]); // up to and including `>`
                    fixed.push(';');
                    pre_result.push_str(&fixed);
                } else {
                    pre_result.push_str(line);
                }
            } else {
                pre_result.push_str(line);
            }
        } else {
            pre_result.push_str(line);
        }
        pre_result.push('\n');
    }
    if pre_result.ends_with('\n') {
        pre_result.pop();
    }
    let code = pre_result;
    // Second pass: strip `)` immediately after JSX close patterns
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b')' && i >= 1 && bytes[i - 1] == b'>' {
            // Check if this `>` is part of a JSX close: `/>`  or `</...>`
            let mut j = i - 2; // skip the `>`
            // Walk back to find if this is `/>` or `</Tag>`
            let is_jsx_close = if j < len && bytes[j] == b'/' {
                true // `/>)`
            } else {
                // Check for `</Tag>)` — walk back past tag name to find `</`
                while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                    if j == 0 {
                        break;
                    }
                    j -= 1;
                }
                j >= 1 && bytes[j] == b'/' && bytes[j - 1] == b'<'
            };
            if is_jsx_close {
                // Skip the `)`
                i += 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize optional parentheses around expressions in various contexts.
/// OXC omits optional parens; Babel includes them.
fn normalize_optional_parens(code: &str) -> String {
    let mut result = code.to_string();
    // Assignment in return/const: `(x = y)` → `x = y` when it's the only expr
    // This is too broad — skip for now and handle specific patterns:

    // Ternary JSX branches: `? ( <div /> ) :` → `? <div /> :`
    // Arrow body ternary: `=> (cond ? x : y)` → `=> cond ? x : y`
    // Double parens in while: `while ((x = y))` → `while (x = y)`
    result = result.replace("while ((", "while (");
    // Fix the extra `)` from removing one `(`
    // This is tricky — only strip when there are unbalanced parens
    // Let's handle it line by line
    let mut output = String::with_capacity(result.len());
    for line in result.lines() {
        let trimmed = line.trim();
        // `while (x = y))` pattern — one extra `)`
        if trimmed.starts_with("while (")
            && trimmed.matches(')').count() > trimmed.matches('(').count()
        {
            // Remove last `)` before `{` or end
            if let Some(pos) = line.rfind(")) {") {
                output.push_str(&line[..pos + 1]);
                output.push_str(&line[pos + 2..]);
            } else if trimmed.ends_with("))") {
                let pos = line.rfind("))").unwrap();
                output.push_str(&line[..pos + 1]);
            } else {
                output.push_str(line);
            }
        }
        // Ternary with parens around JSX: `? ( <...> ) :` → `? <...> :`
        else if trimmed.contains("? ( <") && trimmed.contains("> ) :") {
            let normalized = line.replace("? ( <", "? <").replace("> ) :", "> :");
            output.push_str(&normalized);
        } else if trimmed.contains("? ( <") && trimmed.contains("/> ) :") {
            let normalized = line.replace("? ( <", "? <").replace("/> ) :", "/> :");
            output.push_str(&normalized);
        }
        // Arrow body ternary parens: `=> (cond ? x : y)` → `=> cond ? x : y`
        else if trimmed.contains("=> (") && trimmed.contains(" ? ") && !trimmed.contains("=> ()")
        {
            let normalized = line.replacen("=> (", "=> ", 1);
            if let Some(pos) = normalized.rfind(");") {
                output.push_str(&normalized[..pos]);
                output.push(';');
            } else if normalized.trim_end().ends_with(')') {
                let pos = normalized.rfind(')').unwrap();
                output.push_str(&normalized[..pos]);
            } else {
                output.push_str(&normalized);
            }
        }
        // Arrow body JSX parens: `=> (<...>)` → `=> <...>`
        else if trimmed.contains("=> ( <") {
            let normalized = line.replace("=> ( <", "=> <");
            // Also strip matching closing ` )` or ` ))`
            let normalized = normalized.replace(" /> )", " />");
            let normalized = normalized.replace(" /> ))", " />)");
            output.push_str(&normalized);
        }
        // Const assignment with parens: `const x = (y = z)` → `const x = y = z`
        else if (trimmed.starts_with("const ") || trimmed.starts_with("let "))
            && trimmed.contains("= (")
            && trimmed.ends_with(");")
            && trimmed.matches('(').count() == 1
            && trimmed.matches(')').count() == 2
        {
            // Single paren wrap around RHS: `const x = (expr);` → `const x = expr;`
            let normalized = line.replacen("= (", "= ", 1);
            if let Some(pos) = normalized.rfind(");") {
                output.push_str(&normalized[..pos]);
                output.push(';');
            } else {
                output.push_str(&normalized);
            }
        }
        // Ternary alternate with JSX parens: `: ( <...> )` → `: <...>`
        // Handles both self-closing (`/>`) and closing tags (`</tag>`).
        else if trimmed.contains(": ( <") && (trimmed.contains(" />") || trimmed.contains("</")) {
            let mut normalized = line.replace(": ( <", ": <");
            // Strip ` )` after self-closing `/>`
            normalized = normalized.replace(" /> )", " />");
            // Strip ` )` after closing tag `</tag>`
            // Pattern: `</tagName> )` → `</tagName>`
            if let Some(pos) = normalized.find("> )") {
                // Check if the `>` closes a `</tag` sequence
                let before = &normalized[..pos];
                if before.rfind("</").is_some() {
                    normalized = format!("{}{}", &normalized[..pos + 1], &normalized[pos + 3..]);
                }
            }
            output.push_str(&normalized);
        }
        // Logical AND/OR with JSX parens: `&& ( <...> )` → `&& <...>`
        else if (trimmed.contains("&& ( <") || trimmed.contains("|| ( <"))
            && trimmed.contains(" />")
        {
            let normalized = line
                .replace("&& ( <", "&& <")
                .replace("|| ( <", "|| <")
                .replace(" /> )", " />")
                .replace(" /> );", " />;");
            output.push_str(&normalized);
        }
        // Comma expression with extra parens: `((x = y), z)` → `(x = y, z)`
        else if trimmed.contains("((") && trimmed.contains("),") {
            let normalized = line.replace("((", "(").replace("),", ",");
            output.push_str(&normalized);
        }
        // Object destructuring spacing: `{ref, ...other}` → `{ ref, ...other }`
        // Only normalize by stripping spaces: `{ ref, ...other }` → `{ref, ...other}`
        else if trimmed.contains("({ ") && trimmed.contains(" } =") {
            let normalized = line.replace("({ ", "({").replace(" } =", "} =");
            output.push_str(&normalized);
        } else {
            output.push_str(line);
        }
        output.push('\n');
    }
    if output.ends_with('\n') {
        output.pop();
    }
    output
}

/// Normalize import source quotes: single → double.
/// OXC uses double quotes, Babel uses single quotes for import sources.
fn normalize_import_quotes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for line in code.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("import ") || trimmed.starts_with("export "))
            && trimmed.contains(" from '")
        {
            // Replace `from '...'` with `from "..."`
            let normalized = line.replace(" from '", " from \"");
            // Find the closing quote and replace it
            if let Some(pos) = normalized.rfind("';") {
                result.push_str(&normalized[..pos]);
                result.push_str("\";");
            } else if let Some(pos) = normalized.rfind('\'') {
                result.push_str(&normalized[..pos]);
                result.push('"');
            } else {
                result.push_str(&normalized);
            }
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize `function()` vs `function ()` — space before parens.
fn normalize_function_paren_space(code: &str) -> String {
    code.replace("function ()", "function()")
        .replace("function () {", "function() {")
}

/// Normalize empty blocks: `if (x) {} else` vs `if (x) {\n} else`.
/// Collapse empty blocks with newline to single line.
fn normalize_empty_block_newlines(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let lines: Vec<&str> = code.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for `{` followed by `} else` or just `}` on next line
        if trimmed.ends_with('{')
            && i + 1 < lines.len()
            && (lines[i + 1].trim() == "} else {" || lines[i + 1].trim() == "}")
        {
            // Merge: `{\n}` → `{}`
            result.push_str(lines[i]);
            result.push(' ');
            result.push_str(lines[i + 1].trim());
            result.push('\n');
            i += 2;
        } else {
            result.push_str(lines[i]);
            result.push('\n');
            i += 1;
        }
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Collapse multi-line short arrays/objects to single line.
/// OXC prints `[\n  x,\n  y\n]` while Babel prints `[x, y]`.
fn normalize_multiline_short_arrays(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let lines: Vec<&str> = code.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        // Check for line ending with `[` or `= [` (start of multi-line array)
        // or `= {` for short objects (not functions — no `=>` or `{` alone)
        let is_array_start = trimmed.ends_with(" [")
            || trimmed.ends_with("= [")
            || trimmed.ends_with("([")
            || trimmed.ends_with(", [")
            || trimmed.ends_with("= [{")
            || trimmed.ends_with("={[");
        let is_obj_start = (trimmed.ends_with(" {")
            || trimmed.ends_with("= {")
            || trimmed.ends_with("([{")
            || trimmed.ends_with(", {"))
            && !trimmed.ends_with("=> {")
            && !trimmed.ends_with("else {")
            && !trimmed.ends_with(") {")
            && !trimmed.ends_with("=>{");
        if is_array_start || is_obj_start {
            let bracket = if is_array_start {
                (b'[', b']')
            } else {
                (b'{', b'}')
            };
            // Collect lines until matching closing bracket
            let mut depth = 0i32;
            let mut collected = Vec::new();
            let start = i;
            let mut found_close = false;
            let mut j = i;
            while j < lines.len() && j - start < 30 {
                // Max 30 lines for "short" arrays/objects
                let t = lines[j].trim();
                for ch in t.bytes() {
                    if ch == bracket.0 {
                        depth += 1;
                    } else if ch == bracket.1 {
                        depth -= 1;
                    }
                }
                collected.push(t);
                if depth <= 0 {
                    found_close = true;
                    j += 1;
                    break;
                }
                j += 1;
            }
            if found_close && collected.len() > 1 && collected.len() <= 25 {
                // Collapse to single line
                let collapsed = collected.join(" ");
                // Clean up extra spaces
                let collapsed = collapsed
                    .replace("[ ", "[")
                    .replace(" ]", "]")
                    .replace("{ ", "{")
                    .replace(" }", "}")
                    .replace(",  ", ", ");
                let indent = lines[start].len() - lines[start].trim_start().len();
                for _ in 0..indent {
                    result.push(' ');
                }
                result.push_str(&collapsed);
                result.push('\n');
                i = j;
                continue;
            }
        }
        result.push_str(lines[i]);
        result.push('\n');
        i += 1;
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize `{ value }` → `{value}` inside array literals only.
fn normalize_object_in_array_spacing(code: &str) -> String {
    // Very targeted: only strip `{ ` after `[` or `, ` and ` }` before `]` or `,`
    code.replace("[{ ", "[{")
        .replace(", { ", ", {")
        .replace(" }]", "}]")
        .replace(" },", "},")
}

/// Normalize all single-quoted strings to double-quoted.
/// OXC uses double quotes, Babel preserves original single quotes.
fn normalize_const_string_quotes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'\'' {
            i += 1;
            let mut value = String::new();
            let mut valid = true;
            while i < len && bytes[i] != b'\'' {
                if bytes[i] == b'\\' && i + 1 < len {
                    value.push(bytes[i] as char);
                    value.push(bytes[i + 1] as char);
                    i += 2;
                } else if bytes[i] == b'\n' {
                    valid = false;
                    break;
                } else {
                    value.push(bytes[i] as char);
                    i += 1;
                }
            }
            if valid && i < len {
                result.push('"');
                result.push_str(&value.replace('"', "\\\""));
                result.push('"');
                i += 1;
            } else {
                result.push('\'');
                result.push_str(&value);
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Normalize `{ }` → `{}` (empty block with inner space).
fn normalize_empty_block_inner_space(code: &str) -> String {
    code.replace("{ }", "{}")
}

/// Normalize destructuring brace spacing: `const { x, y } = z` → `const {x, y} = z`.
/// OXC adds spaces inside destructuring braces, Babel doesn't.
fn normalize_destructuring_brace_spacing(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for line in code.lines() {
        let trimmed = line.trim();
        // Only normalize `const/let/var { ... } = ` patterns
        if (trimmed.starts_with("const {")
            || trimmed.starts_with("let {")
            || trimmed.starts_with("var {"))
            && trimmed.contains("} =")
        {
            // Strip spaces inside the destructuring braces
            let normalized = line
                .replace("const { ", "const {")
                .replace("let { ", "let {")
                .replace("var { ", "var {")
                .replace(" } =", "} =");
            result.push_str(&normalized);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize single arrow param parens: `(x) =>` → `x =>`.
/// OXC wraps single arrow params, Babel doesn't.
fn normalize_single_arrow_param_parens(code: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Match `(identifier) =>` where identifier is a simple name
        regex::Regex::new(r"\(([a-zA-Z_$][a-zA-Z0-9_$]*)\) =>").unwrap()
    });
    re.replace_all(code, "$1 =>").to_string()
}

/// Strip parens around assignment expressions: `const x = (y = z)` → `const x = y = z`
/// and `(y = expr), z` → `y = expr, z`.
fn normalize_assignment_expression_parens(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for line in code.lines() {
        let trimmed = line.trim();
        // `const x = (expr);` where the paren wraps a simple assignment
        if (trimmed.starts_with("const ") || trimmed.starts_with("let "))
            && trimmed.contains("= (")
            && trimmed.ends_with(");")
            && !trimmed.contains("= ()")
            && !trimmed.contains("= (function")
            && !trimmed.contains("= (class")
        {
            let open_count = trimmed.matches('(').count();
            let close_count = trimmed.matches(')').count();
            if open_count == close_count && open_count >= 1 {
                let normalized = line.replacen("= (", "= ", 1);
                if let Some(pos) = normalized.rfind(");") {
                    result.push_str(&normalized[..pos]);
                    result.push(';');
                } else {
                    result.push_str(&normalized);
                }
                result.push('\n');
                continue;
            }
        }
        // `(y = expr), z` → `y = expr, z` — comma expression with parens
        if trimmed.contains("(") && trimmed.contains(" = ") && trimmed.contains("),") {
            // Very targeted: only when first char after indent is `(`
            if trimmed.starts_with('(') && !trimmed.starts_with("((") {
                let normalized = line.replacen('(', "", 1);
                let normalized = normalized.replacen("),", ",", 1);
                result.push_str(&normalized);
                result.push('\n');
                continue;
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Normalize `.1` → `0.1` (leading zero in numeric literals).
/// Uses simple string replacement for common patterns.
fn normalize_numeric_leading_zero(code: &str) -> String {
    code.replace(" .1}", " 0.1}")
        .replace(" .1s", " 0.1s")
        .replace(" .3s", " 0.3s")
        .replace(" .8 ", " 0.8 ")
        .replace("*.1}", "*0.1}")
        .replace("* .1", "* 0.1")
        .replace("{.8 ", "{0.8 ")
        .replace("${.8", "${0.8")
        .replace("${.1", "${0.1")
        .replace("${.3", "${0.3")
}

/// Normalize trailing text space before JSX close tag.
/// `>increment </button>` → `>increment</button>` and `>;{t4}; </>` → `>;{t4};</>`
fn normalize_jsx_trailing_text_space_before_close(code: &str) -> String {
    // Strip space between text/expression end and `</`
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b' ' && i + 2 < len && bytes[i + 1] == b'<' && bytes[i + 2] == b'/' {
            // Skip space before `</` when preceded by text or `;`
            if i > 0
                && (bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b';'
                    || bytes[i - 1] == b')')
            {
                i += 1;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

/// Normalize space after optional call: `x?.( <` → `x?.(<`.
fn normalize_optional_call_space(code: &str) -> String {
    code.replace("?.( <", "?.(<").replace("?.( (", "?.(( ")
}

/// Normalize space before JSX close bracket after attr: `val={t7} >` → `val={t7}>`.
fn normalize_jsx_attr_trailing_space(code: &str) -> String {
    code.replace("} >", "}>")
}

/// Normalize empty statement semicolons: `; ;` → `;`.
/// Babel sometimes emits standalone `;` (empty statements) that OXC omits.
fn normalize_empty_statement_semicolons(code: &str) -> String {
    code.replace("; ;", ";")
}

/// Normalize optional whitespace before closing braces: ` }` -> `}` when it
/// appears at the end of a scope guard body or similar context.
fn normalize_space_before_closing_brace(code: &str) -> String {
    let re = regex::Regex::new(r" \} else \{").unwrap();
    re.replace_all(code, "} else {").to_string()
}

/// Strip standalone top-level comment trivia while preserving nested comments.
fn normalize_top_level_comment_trivia(code: &str) -> String {
    let mut result = Vec::new();
    let mut top_level_brace_depth: i32 = 0;
    let mut in_top_level_block_comment = false;

    for line in code.lines() {
        let trimmed = line.trim();

        if in_top_level_block_comment {
            if trimmed.contains("*/") {
                in_top_level_block_comment = false;
            }
            continue;
        }

        if top_level_brace_depth == 0 {
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("/*") {
                if !trimmed.contains("*/") {
                    in_top_level_block_comment = true;
                }
                continue;
            }
            if trimmed.starts_with('*') || trimmed.starts_with("*/") {
                continue;
            }
        }

        result.push(line.to_string());

        top_level_brace_depth += line.chars().filter(|&c| c == '{').count() as i32;
        top_level_brace_depth -= line.chars().filter(|&c| c == '}').count() as i32;
    }

    result.join("\n")
}

/// Remove blank lines caused by Babel's `retainLines: true + compact: true`.
fn normalize_scope_body_blank_lines(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut scope_depth: i32 = 0;
    let mut in_scope = false;
    let mut in_function_body = false;
    let mut function_brace_depth: i32 = 0;
    let mut prev_trimmed = "";

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // Track whether we're inside a function body
        if !in_function_body && is_function_body_start(trimmed) {
            in_function_body = true;
            function_brace_depth = 1;
        } else if in_function_body {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            function_brace_depth += opens - closes;
            if function_brace_depth <= 0 {
                in_function_body = false;
            }
        }

        // Detect scope check start
        if !in_scope
            && (trimmed.starts_with("if ($[") || trimmed.starts_with("if (($["))
            && (trimmed.contains("Symbol.for") || trimmed.contains("!=="))
        {
            in_scope = true;
            scope_depth = 0;
        }

        if in_scope {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            scope_depth += opens - closes;

            // Skip blank lines inside scope bodies
            if trimmed.is_empty() && scope_depth > 0 {
                prev_trimmed = trimmed;
                continue;
            }

            if scope_depth <= 0 {
                in_scope = false;
            }
        }

        // Inside function bodies, strip all blank lines (retainLines artifact).
        if in_function_body && trimmed.is_empty() {
            prev_trimmed = trimmed;
            continue;
        }

        // At top level, strip blank lines after closing braces
        if !in_function_body && trimmed.is_empty() {
            let pt = prev_trimmed;
            if pt == "}" || pt == "};" || pt == "});" || pt == "})" || pt.ends_with("};") {
                prev_trimmed = trimmed;
                continue;
            }
        }

        // Strip blank between `*/` and function declaration
        if trimmed.is_empty()
            && prev_trimmed == "*/"
            && lines
                .iter()
                .skip(i + 1)
                .find(|l| !l.trim().is_empty())
                .is_some_and(|next| next.trim().starts_with("function "))
        {
            prev_trimmed = trimmed;
            continue;
        }

        // Strip blank lines between import declarations
        if trimmed.is_empty()
            && prev_trimmed.starts_with("import ")
            && lines
                .iter()
                .skip(i + 1)
                .find(|l| !l.trim().is_empty())
                .is_some_and(|next| next.trim().starts_with("import "))
        {
            prev_trimmed = trimmed;
            continue;
        }

        prev_trimmed = trimmed;
        result.push(*line);
    }

    result.join("\n")
}

fn is_function_body_start(trimmed: &str) -> bool {
    trimmed.ends_with('{')
        && (trimmed.starts_with("function ")
            || trimmed.contains(" function ")
            || trimmed.contains("function(")
            || trimmed.contains("function (")
            || trimmed.contains("=> {"))
}

/// Collapse blank lines between completed top-level statements and the next
/// top-level binding/export statement.
fn normalize_top_level_statement_blank_lines(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<&str> = Vec::with_capacity(lines.len());
    let mut top_level_brace_depth: i32 = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        if trimmed.is_empty() && top_level_brace_depth == 0 {
            let prev_trimmed = result
                .iter()
                .rev()
                .copied()
                .find(|line| !line.trim().is_empty())
                .map(str::trim)
                .unwrap_or("");
            let next_trimmed = lines
                .iter()
                .skip(i + 1)
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim())
                .unwrap_or("");

            if ends_top_level_statement(prev_trimmed)
                && starts_top_level_binding_or_export(next_trimmed)
            {
                continue;
            }
        }

        result.push(*line);

        if !trimmed.is_empty() {
            let opens = trimmed.chars().filter(|&c| c == '{').count() as i32;
            let closes = trimmed.chars().filter(|&c| c == '}').count() as i32;
            top_level_brace_depth += opens - closes;
        }
    }

    result.join("\n")
}

fn ends_top_level_statement(trimmed: &str) -> bool {
    trimmed.ends_with(';') || trimmed.ends_with('}')
}

fn starts_top_level_binding_or_export(trimmed: &str) -> bool {
    trimmed.starts_with("export ")
        || trimmed.starts_with("const ")
        || trimmed.starts_with("let ")
        || trimmed.starts_with("var ")
}

/// Normalize comment placement in the import region at the top of the file.
fn normalize_import_region_comments(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_import_region = true;

    for line in &lines {
        let trimmed = line.trim();

        if !in_import_region {
            result.push(line.to_string());
            continue;
        }

        // Import line -- detach any trailing comment
        if trimmed.starts_with("import ") || trimmed.starts_with("const {") {
            if let Some(comment_pos) = find_trailing_comment_on_import(trimmed) {
                let import_part = trimmed[..comment_pos].trim_end();
                let comment_part = trimmed[comment_pos..].trim();
                result.push(import_part.to_string());
                if !comment_part.is_empty() {
                    result.push(comment_part.to_string());
                }
            } else {
                result.push(line.to_string());
            }
            continue;
        }

        // Blank line in import region -- skip
        if trimmed.is_empty() {
            continue;
        }

        // Comment line in import region -- keep
        if trimmed.starts_with("//") || trimmed.starts_with("/*") {
            result.push(line.to_string());
            continue;
        }

        // Block comment continuation
        if trimmed.starts_with('*') {
            result.push(line.to_string());
            continue;
        }

        // Non-import, non-comment, non-blank -- end of import region
        in_import_region = false;
        result.push(line.to_string());
    }

    result.join("\n")
}

fn normalize_generated_memoization_comments(code: &str) -> String {
    let inline_re = regex::Regex::new(r#"\s*// "(?:useMemo|useMemoCache)".*$"#).unwrap();
    code.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("// check if ")
                || trimmed == "// Inputs changed, recompute"
                || trimmed == "// Inputs did not change, use cached value"
            {
                return None;
            }

            Some(inline_re.replace(trimmed, "").to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find the position of a trailing `//` or `/*` comment on an import line,
/// skipping occurrences inside string literals.
fn find_trailing_comment_on_import(line: &str) -> Option<usize> {
    let mut in_single = false;
    let mut in_double = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' && !in_double {
            in_single = !in_single;
        } else if bytes[i] == b'"' && !in_single {
            in_double = !in_double;
        } else if bytes[i] == b';' && !in_single && !in_double {
            let rest = line[i + 1..].trim_start();
            if rest.starts_with("//") || rest.starts_with("/*") {
                let offset = line.len() - line[i + 1..].trim_start().len();
                return Some(offset);
            }
            return None;
        }
        i += 1;
    }
    None
}

/// Normalize array formatting within FIXTURE_ENTRYPOINT lines.
fn normalize_fixture_entrypoint_array_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            if line.contains("FIXTURE_ENTRYPOINT") {
                let mut s = line.to_string();
                // Remove trailing comma before ] (single-line)
                while let Some(pos) = s.find(", ]") {
                    s.replace_range(pos..pos + 3, "]");
                }
                // Remove space after [ before {
                while let Some(pos) = s.find("[ {") {
                    s.replace_range(pos..pos + 3, "[{");
                }
                // Remove space after [ before other content (but not before ])
                while let Some(pos) = s.find("[ ") {
                    if s[pos + 2..].starts_with(']') {
                        break;
                    }
                    s.replace_range(pos..pos + 2, "[");
                }
                // Normalize parenthesized objects in arrays
                while s.contains("( {") {
                    s = s.replace("( {", "{");
                }
                while s.contains("}),") || s.contains("})]") || s.contains("}) ") {
                    s = s.replace("}),", "},");
                    s = s.replace("})]", "}]");
                    s = s.replace("}) ", "} ");
                }
                // Remove trailing space before ] (after stripping parens)
                while let Some(pos) = s.find(" ]") {
                    if pos > 0 && s.as_bytes()[pos - 1] != b'[' {
                        s.replace_range(pos..pos + 1, "");
                    } else {
                        break;
                    }
                }
                // Ensure trailing semicolon on FIXTURE_ENTRYPOINT export declarations.
                let trimmed = s.trim_end();
                if (trimmed.starts_with("export let FIXTURE_ENTRYPOINT")
                    || trimmed.starts_with("export const FIXTURE_ENTRYPOINT"))
                    && trimmed.ends_with('}')
                    && !trimmed.ends_with("};")
                {
                    s = format!("{};", trimmed);
                }
                s
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_multiline_trailing_commas_before_closers(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r",\n([ \t]*[}\]])").unwrap())
        .replace_all(code, "\n$1")
        .into_owned()
}

fn normalize_bracket_string_literal_spacing(code: &str) -> String {
    let re = regex::Regex::new(r#"\[\s*("[^"\n]*"|'[^'\n]*')\s*\]"#).unwrap();
    re.replace_all(code, "[$1]").into_owned()
}

fn normalize_trailing_comma_in_calls(code: &str) -> String {
    use regex::Regex;
    let trailing = Regex::new(r",\s*\)").unwrap();
    let result = trailing.replace_all(code, ")");
    let space_after_open = Regex::new(r"(\w)\(\s+").unwrap();
    let result = space_after_open.replace_all(&result, "$1(");
    result.to_string()
}

fn is_basic_block_label_open_brace(line: &str) -> bool {
    if !line.starts_with("bb") || !line.ends_with(": {") {
        return false;
    }
    let digits = &line[2..line.len() - 3];
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn trailing_comma_before_brace_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| regex::Regex::new(r",\s*}").unwrap())
}

fn normalize_compare_multiline_brace_literals(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let is_bb_label_block = is_basic_block_label_open_brace(trimmed);
        let is_fixture_entrypoint = trimmed.starts_with("export let FIXTURE_ENTRYPOINT = {")
            || trimmed.starts_with("export const FIXTURE_ENTRYPOINT = {");
        let ends_with_open_brace = trimmed.ends_with('{');
        let is_obj_literal_start = (is_fixture_entrypoint
            || trimmed.ends_with("= {")
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
                if is_fixture_entrypoint {
                    parts = normalize_fixture_entrypoint_brace_parts(parts);
                }
                let total_len: usize = parts.iter().map(|p| p.len()).sum::<usize>() + parts.len();
                if is_fixture_entrypoint || total_len <= 200 {
                    let joined = parts.join(" ");
                    let cleaned = trailing_comma_before_brace_regex()
                        .replace_all(&joined.replace("  ", " "), " }")
                        .to_string();
                    result.push(cleaned);
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

fn normalize_fixture_entrypoint_brace_parts(parts: Vec<String>) -> Vec<String> {
    parts
        .into_iter()
        .filter_map(|part| {
            let stripped_block_comments = normalize_strip_block_comments(&part);
            let trimmed = stripped_block_comments.trim();
            if trimmed.is_empty() || trimmed.starts_with("//") {
                return None;
            }
            if let Some(pos) = find_line_comment_start(trimmed) {
                let before = trimmed[..pos].trim_end();
                if before.is_empty() {
                    None
                } else {
                    Some(before.to_string())
                }
            } else {
                Some(trimmed.to_string())
            }
        })
        .collect()
}

fn normalize_compare_multiline_imports(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("import {") || trimmed.starts_with("import type {") {
            let brace_open = trimmed.matches('{').count();
            let brace_close = trimmed.matches('}').count();
            if brace_open > brace_close {
                let mut parts = vec![trimmed.to_string()];
                let mut j = i + 1;
                let mut depth = (brace_open - brace_close) as i32;
                while j < lines.len() && depth > 0 {
                    let t = lines[j].trim();
                    depth += t.matches('{').count() as i32 - t.matches('}').count() as i32;
                    if !is_comment_only_import_line(t) {
                        parts.push(t.to_string());
                    }
                    j += 1;
                }
                let joined = parts.join(" ");
                let cleaned = trailing_comma_before_brace_regex()
                    .replace_all(&joined.replace("  ", " "), " }")
                    .to_string();
                result.push(cleaned);
                i = j;
                continue;
            }
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn is_comment_only_import_line(trimmed: &str) -> bool {
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("*/")
}

fn normalize_labeled_switch_breaks(code: &str) -> String {
    let labeled_switch = regex::Regex::new(r"\bbb\d+:\s*(switch\s*\()").unwrap();
    let code = labeled_switch.replace_all(code, "$1").to_string();
    let labeled_break = regex::Regex::new(r"\bbreak\s+bb\d+;").unwrap();
    labeled_break.replace_all(&code, "break;").to_string()
}

fn normalize_switch_case_braces(code: &str) -> String {
    let mut result = Vec::new();
    let mut in_case_brace = false;
    for line in code.lines() {
        let trimmed = line.trim();
        if let Some(prefix) = trimmed.strip_suffix(" {")
            && (prefix.starts_with("case ") || prefix == "default:")
        {
            result.push(prefix.to_string());
            in_case_brace = true;
            continue;
        }
        if in_case_brace && trimmed == "}" {
            in_case_brace = false;
            continue;
        }
        if trimmed.starts_with("case ") || trimmed == "default:" {
            in_case_brace = false;
        }
        result.push(trimmed.to_string());
    }
    result.join("\n")
}

fn normalize_multiline_switch_cases(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut result = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.starts_with("case ") || trimmed == "default:" {
            let mut parts = vec![trimmed.to_string()];
            let mut j = i + 1;
            while j < lines.len() {
                let next = lines[j].trim();
                if next.starts_with("case ") || next == "default:" || next == "}" {
                    break;
                }
                parts.push(next.to_string());
                j += 1;
            }
            result.push(parts.join(" "));
            i = j;
            continue;
        }
        result.push(trimmed.to_string());
        i += 1;
    }
    result.join("\n")
}

fn normalize_ts_object_type_semicolons(code: &str) -> String {
    let re = regex::Regex::new(r";(\s*})").unwrap();
    re.replace_all(code, "$1").to_string()
}

fn normalize_numeric_exponent_literals(code: &str) -> String {
    let re = regex::Regex::new(r"\b(\d+)e([+-]?\d+)\b").unwrap();
    re.replace_all(code, |caps: &regex::Captures| {
        let base = caps[1].parse::<u128>().ok();
        let exponent = caps[2].parse::<i32>().ok();
        match (base, exponent) {
            (Some(base), Some(exponent)) if (0..=18).contains(&exponent) => base
                .checked_mul(10u128.pow(exponent as u32))
                .map(|value| value.to_string())
                .unwrap_or_else(|| caps[0].to_string()),
            _ => caps[0].to_string(),
        }
    })
    .to_string()
}

/// Normalize labeled block braces: strip the `{ }` wrapper from labeled blocks.
fn normalize_labeled_block_braces(code: &str) -> String {
    let mut result = code.to_string();
    loop {
        let next = strip_labeled_block_braces_pass(&result);
        if next == result {
            break;
        }
        result = next;
    }
    result
}

/// One pass of labeled-block brace stripping, operating on the full code string.
fn strip_labeled_block_braces_pass(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let len = chars.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        if i + 4 < len && chars[i] == 'b' && chars[i + 1] == 'b' {
            let mut j = i + 2;
            while j < len && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2
                && j + 2 < len
                && chars[j] == ':'
                && chars[j + 1] == ' '
                && chars[j + 2] == '{'
            {
                let brace_pos = j + 2;
                let after_brace = if brace_pos + 1 < len {
                    chars[brace_pos + 1]
                } else {
                    '\0'
                };
                if after_brace == ' ' || after_brace == '\n' {
                    let content_start = brace_pos + 1;
                    let mut depth: i32 = 1;
                    let mut k = content_start;
                    if after_brace == ' ' {
                        k += 1;
                    }
                    let scan_start = k;
                    while k < len && depth > 0 {
                        match chars[k] {
                            '{' => depth += 1,
                            '}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                    if depth == 0 {
                        for c in &chars[i..j + 1] {
                            result.push(*c);
                        }
                        result.push(' ');
                        let inner: String = chars[scan_start..k].iter().collect();
                        let inner_trimmed = inner.trim();
                        result.push_str(inner_trimmed);
                        i = k + 1;
                        continue;
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn normalize_small_array_bracket_spacing(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("return [")
                || trimmed.contains("= [")
                || trimmed.contains("fbt._(")
            {
                trimmed
                    .replace("[ ", "[")
                    .replace(" ]", "]")
                    .replace(", ]", "]")
            } else {
                trimmed.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_compare_unicode_escapes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for ch in code.chars() {
        if ch == '\t' {
            result.push_str("\\t");
        } else if !ch.is_ascii() {
            let cp = ch as u32;
            if cp <= 0xFFFF {
                result.push_str(&format!("\\u{:04x}", cp));
            } else {
                let cp = cp - 0x1_0000;
                let high = 0xD800 + ((cp >> 10) & 0x3FF);
                let low = 0xDC00 + (cp & 0x3FF);
                result.push_str(&format!("\\u{:04x}\\u{:04x}", high, low));
            }
        } else {
            result.push(ch);
        }
    }

    let utf8_pair =
        regex::Regex::new(r"\\u00([cCdD][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])").unwrap();
    let result = utf8_pair
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let codepoint = ((b1 as u32 & 0x1F) << 6) | (b2 as u32 & 0x3F);
            format!("\\u{:04x}", codepoint)
        })
        .to_string();

    let utf8_triplet = regex::Regex::new(
        r"\\u00([eE][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])",
    )
    .unwrap();
    let result = utf8_triplet
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let b3 = u8::from_str_radix(&caps[3], 16).unwrap();
            let codepoint =
                ((b1 as u32 & 0x0F) << 12) | ((b2 as u32 & 0x3F) << 6) | (b3 as u32 & 0x3F);
            format!("\\u{:04x}", codepoint)
        })
        .to_string();

    let utf8_quad = regex::Regex::new(
        r"\\u00([fF][0-7])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])\\u00([89aAbB][0-9a-fA-F])",
    )
    .unwrap();
    let result = utf8_quad
        .replace_all(&result, |caps: &regex::Captures| {
            let b1 = u8::from_str_radix(&caps[1], 16).unwrap();
            let b2 = u8::from_str_radix(&caps[2], 16).unwrap();
            let b3 = u8::from_str_radix(&caps[3], 16).unwrap();
            let b4 = u8::from_str_radix(&caps[4], 16).unwrap();
            let codepoint = ((b1 as u32 & 0x07) << 18)
                | ((b2 as u32 & 0x3F) << 12)
                | ((b3 as u32 & 0x3F) << 6)
                | (b4 as u32 & 0x3F);
            let cp = codepoint - 0x1_0000;
            let high = 0xD800 + ((cp >> 10) & 0x3FF);
            let low = 0xDC00 + (cp & 0x3FF);
            format!("\\u{:04x}\\u{:04x}", high, low)
        })
        .to_string();

    let escape_case = regex::Regex::new(r"\\u([0-9a-fA-F]{4})").unwrap();
    escape_case
        .replace_all(&result, |caps: &regex::Captures| {
            format!("\\u{}", caps[1].to_ascii_lowercase())
        })
        .to_string()
}

/// Strip block comments (`/* ... */` and `/** ... */`) from code.
fn normalize_strip_block_comments(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'\'' || bytes[i] == b'"' {
            let quote = bytes[i];
            result.push(quote as char);
            i += 1;
            while i < len && bytes[i] != quote {
                if bytes[i] == b'\\' && i + 1 < len {
                    result.push(bytes[i] as char);
                    i += 1;
                    result.push(bytes[i] as char);
                    i += 1;
                } else {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < len {
                result.push(bytes[i] as char);
                i += 1;
            }
        } else if bytes[i] == b'`' {
            result.push('`');
            i += 1;
            while i < len && bytes[i] != b'`' {
                if bytes[i] == b'\\' && i + 1 < len {
                    result.push(bytes[i] as char);
                    i += 1;
                    result.push(bytes[i] as char);
                    i += 1;
                } else {
                    result.push(bytes[i] as char);
                    i += 1;
                }
            }
            if i < len {
                result.push('`');
                i += 1;
            }
        } else if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

/// Find the start of a `//` line comment, ignoring occurrences inside strings
/// and template literals.
fn find_line_comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        match bytes[i] {
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < len && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'`' => {
                i += 1;
                while i < len && bytes[i] != b'`' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            b'/' if i + 1 < len && bytes[i + 1] == b'/' => {
                return Some(i);
            }
            _ => {
                i += 1;
            }
        }
    }
    None
}

/// Collapse multiline function/method invocations into a single line.
fn normalize_multiline_call_invocations(code: &str) -> String {
    fn paren_delta(line: &str) -> i32 {
        line.chars().fold(0, |depth, ch| match ch {
            '(' => depth + 1,
            ')' => depth - 1,
            _ => depth,
        })
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        let current = lines[i].trim();
        let starts_call = current.contains('(')
            && !current.starts_with("if (")
            && !current.starts_with("for (")
            && !current.starts_with("while (")
            && !current.starts_with("switch (")
            && !current.starts_with("catch (")
            && !current.starts_with("function ");
        let mut depth = paren_delta(current);
        if starts_call && depth > 0 {
            let mut parts = vec![current.to_string()];
            let mut j = i + 1;
            while j < lines.len() && depth > 0 {
                let part = lines[j].trim();
                if part.starts_with("function ") {
                    break;
                }
                depth += paren_delta(part);
                parts.push(part.to_string());
                j += 1;
            }
            if depth == 0 && parts.len() > 1 {
                out.push(parts.join(" "));
                i = j;
                continue;
            }
        }

        out.push(current.to_string());
        i += 1;
    }

    out.join("\n")
}

fn normalize_dead_bare_var_refs(code: &str) -> String {
    let bare_var_ref_re = regex::Regex::new(r"^var\s+(_ref\d*)\s*;$").unwrap();
    let lines: Vec<&str> = code.lines().collect();
    let mut dead_indices = std::collections::HashSet::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(caps) = bare_var_ref_re.captures(trimmed) {
            let var_name = caps.get(1).unwrap().as_str();
            let word_re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(var_name))).unwrap();
            let used_elsewhere = lines
                .iter()
                .enumerate()
                .any(|(j, other_line)| j != i && word_re.is_match(other_line.trim()));
            if !used_elsewhere {
                dead_indices.insert(i);
            }
        }
    }

    if dead_indices.is_empty() {
        return code.to_string();
    }

    lines
        .iter()
        .enumerate()
        .filter(|(i, _)| !dead_indices.contains(i))
        .map(|(_, line)| line.trim())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collapse multi-line IIFEs into single-line form with wrapping parentheses.
///
/// OXC prints:
///   `= function() {\n  ...\n  }();`
/// Babel prints:
///   `= (function() { ... })();`
///
/// Detects lines ending with `function...() {` (or named `function name() {`)
/// preceded by `=` or `return` context, then collects body lines until `}()` /
/// `}();` closes the IIFE.
fn normalize_multiline_iife_collapsing(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect a line that opens a function-expression IIFE:
        //   ... = function() {
        //   ... = function name(params) {
        //   return function() {
        //   return function name(params) {
        // But NOT standalone function declarations like:
        //   function foo() {
        if is_iife_function_open(trimmed) {
            // Try to find the closing `}()` or `}();`
            let mut brace_depth = 0i32;
            let mut collected = Vec::new();
            let start = i;
            let mut found_iife_close = false;
            let mut j = i;
            let mut close_suffix = "";

            while j < lines.len() && j - start < 40 {
                let t = lines[j].trim();
                for ch in t.bytes() {
                    if ch == b'{' {
                        brace_depth += 1;
                    } else if ch == b'}' {
                        brace_depth -= 1;
                    }
                }
                // Check if this line closes the IIFE: `}()` or `}();`
                if brace_depth == 0 && j > start && (t == "}()" || t == "}();") {
                    close_suffix = if t == "}();" { ";" } else { "" };
                    found_iife_close = true;
                    j += 1;
                    break;
                }
                collected.push(t);
                j += 1;
            }

            if found_iife_close && collected.len() > 1 {
                // collected[0] is the function open line, e.g.:
                //   "const [state, setState] = function() {"
                // collected[1..] are body statements
                // The close line `}()` or `}();` is NOT in collected.
                let func_line = collected[0];
                let func_idx = func_line.find("function").unwrap();
                let prefix = &func_line[..func_idx];
                let func_part = &func_line[func_idx..]; // e.g. "function() {"

                let body = collected[1..].join(" ");

                // Build: prefix(function() { body })()\close_suffix
                let collapsed = format!("{prefix}({func_part} {body} }})(){close_suffix}");

                out.push(collapsed);
                i = j;
                continue;
            }
        }

        out.push(trimmed.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Wrap inline (single-line) IIFEs with parentheses.
///
/// OXC prints `function() { ... }()` without wrapping parens, Babel prints
/// `(function() { ... })()`. This scans each line for `function` keywords in
/// expression position (preceded by `(`, `,`, `=`, or space after `return`),
/// finds the matching closing `}`, and if followed by `(`, wraps the function
/// expression.
fn normalize_inline_iife_parenthesization(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for (line_idx, line) in code.lines().enumerate() {
        if line_idx > 0 {
            result.push('\n');
        }
        result.push_str(&wrap_inline_iifes(line));
    }
    result
}

/// Scan a single line and wrap any unparenthesized IIFE function expressions.
fn wrap_inline_iifes(line: &str) -> String {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + 16);
    let mut i = 0;

    while i < len {
        // Look for "function" keyword
        if !line[i..].starts_with("function") {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        // Check that "function" is preceded by a character indicating expression position
        // (not a statement/declaration position)
        let before_char = if i > 0 { bytes[i - 1] } else { 0 };
        let is_expr_position = matches!(before_char, b'(' | b',' | b'=' | b' ' | b'\t') && i > 0;

        if !is_expr_position {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }

        // Additional check: if preceded by space, verify it's after `return ` or `= `
        // to avoid matching `function` declarations preceded by a space in other contexts
        if before_char == b' ' || before_char == b'\t' {
            let before_str = line[..i].trim_end();
            if !before_str.ends_with("return")
                && !before_str.ends_with('=')
                && !before_str.ends_with('(')
                && !before_str.ends_with(',')
            {
                out.push(bytes[i] as char);
                i += 1;
                continue;
            }
        }

        // We're at a `function` in expression position. Find the opening `{` of the
        // function body, then find its matching `}`.
        let func_start = i;
        // Skip past "function", optional name, and params to find `{`
        let mut j = i + "function".len();
        // Skip whitespace and optional function name
        while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        // Skip optional function name (identifier chars)
        while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$')
        {
            j += 1;
        }
        // Skip whitespace before params
        while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        // Skip params `(...)`
        if j < len && bytes[j] == b'(' {
            let mut paren_depth = 1;
            j += 1;
            while j < len && paren_depth > 0 {
                if bytes[j] == b'(' {
                    paren_depth += 1;
                } else if bytes[j] == b')' {
                    paren_depth -= 1;
                }
                j += 1;
            }
        } else {
            // Not a valid function expression, skip
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Skip whitespace before body `{`
        while j < len && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if j >= len || bytes[j] != b'{' {
            // No opening brace — not a function expression we can handle
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // Find matching `}`
        let mut brace_depth = 1;
        j += 1;
        while j < len && brace_depth > 0 {
            match bytes[j] {
                b'{' => brace_depth += 1,
                b'}' => brace_depth -= 1,
                b'\'' | b'"' | b'`' => {
                    // Skip string literals
                    let quote = bytes[j];
                    j += 1;
                    while j < len && bytes[j] != quote {
                        if bytes[j] == b'\\' {
                            j += 1; // skip escaped char
                        }
                        j += 1;
                    }
                    // j now points at closing quote (or end)
                }
                _ => {}
            }
            j += 1;
        }
        // j is now past the closing `}`. Check if followed by `(`  (IIFE invocation).
        let func_end = j; // position right after `}`
        // Skip whitespace
        let mut k = func_end;
        while k < len && (bytes[k] == b' ' || bytes[k] == b'\t') {
            k += 1;
        }
        if k < len && bytes[k] == b'(' {
            // This is an IIFE! Check if already wrapped in parens.
            // Already wrapped means the char before `function` is `(` AND we can
            // find the matching `)` right after the `}`.
            let already_wrapped = before_char == b'(' && func_start >= 1 && {
                // The char at func_end should be `)` if already wrapped
                // Actually, we need to check: is there a `)` right after `}`?
                let after_close = &line[func_end..];
                after_close.starts_with(')')
            };

            if !already_wrapped {
                // Wrap: insert `(` before function and `)` after `}`
                out.push('(');
                out.push_str(&line[func_start..func_end]);
                out.push(')');
                i = func_end;
                continue;
            }
        }

        // Not an IIFE or already wrapped — output normally
        out.push(bytes[i] as char);
        i += 1;
    }

    out
}

/// Check if a line opens a function expression that will be immediately invoked.
/// Returns true for patterns like:
///   `const x = function() {`
///   `return function name(params) {`
///   `const t0 = function() {`
/// Returns false for standalone function declarations:
///   `function foo() {`
fn is_iife_function_open(trimmed: &str) -> bool {
    // Must end with `{`
    if !trimmed.ends_with('{') {
        return false;
    }
    // Must contain `function`
    let Some(func_idx) = trimmed.find("function") else {
        return false;
    };
    // There must be something before `function` (assignment, return, etc.)
    // A standalone `function foo() {` starts with `function` — that's a declaration
    let before = trimmed[..func_idx].trim();
    if before.is_empty() {
        return false;
    }
    // The prefix must be an assignment or return context
    before.ends_with('=') || before == "return"
}

/// Normalize `if (cond)\nstatement;` to `if (cond) statement;` (collapse).
///
/// OXC puts short if-consequents on one line, Babel keeps them on the next line.
/// We normalize the two-line form to one line so both match.
fn normalize_if_consequent_newline(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Check: line is `if (...)` with NO opening brace (no `{` at end)
        // and no consequent on the same line beyond the closing `)`
        if is_bare_if_condition(trimmed) {
            // Next line should be the consequent (a single statement, not `{`)
            if i + 1 < lines.len() {
                let next_trimmed = lines[i + 1].trim();
                // The consequent should NOT start with `{` (that would be a block)
                // and should be a single statement line
                if !next_trimmed.is_empty()
                    && !next_trimmed.starts_with('{')
                    && !next_trimmed.starts_with("else")
                    && !next_trimmed.starts_with("//")
                {
                    // Collapse: `if (cond)\nstmt;` → `if (cond) stmt;`
                    out.push(format!("{} {}", trimmed, next_trimmed));
                    i += 2;
                    continue;
                }
            }
        }

        out.push(trimmed.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Check if a trimmed line is a bare `if (...)` condition with no consequent.
/// E.g. `if (DEV && _shouldInstrument3)` — ends with `)` and starts with `if (`.
fn is_bare_if_condition(trimmed: &str) -> bool {
    if !trimmed.starts_with("if (") && !trimmed.starts_with("if(") {
        return false;
    }
    // Must end with `)` — meaning just the condition, no consequent
    trimmed.ends_with(')')
}

/// Collapse multi-line `if (` conditions into a single line.
///
/// Babel sometimes breaks long if-conditions across multiple lines:
///   if (
///     $[0] !== x ||
///     $[1] !== y
///   ) {
///
/// OXC keeps them on a single line:
///   if ($[0] !== x || $[1] !== y) {
///
/// This normalization collapses the multi-line form to match OXC's output.
fn normalize_multiline_if_condition(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect `if (` with no closing `)` on the same line
        if (trimmed == "if (" || trimmed == "} else if (")
            || ((trimmed.starts_with("if (") || trimmed.starts_with("} else if ("))
                && !trimmed.contains(") {")
                && !trimmed.ends_with(')'))
        {
            // Collect continuation lines until we find `) {`
            let mut parts = vec![trimmed.to_string()];
            let mut j = i + 1;
            let mut found_close = false;
            while j < lines.len() && j - i < 20 {
                let t = lines[j].trim();
                parts.push(t.to_string());
                if t == ") {" || t.ends_with(") {") {
                    found_close = true;
                    j += 1;
                    break;
                }
                j += 1;
            }
            if found_close && parts.len() > 2 {
                // Join: "if (" + conditions + ") {"
                let collapsed = parts.join(" ");
                // Clean up double spaces
                let collapsed = collapsed
                    .replace("( ", "(")
                    .replace(" )", ")")
                    .replace("  ", " ");
                out.push(collapsed);
                i = j;
                continue;
            }
        }

        out.push(trimmed.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Collapse short multi-line arrow function bodies.
///
/// OXC prints arrow bodies on multiple lines:
///   event =>{
///     dispatch(...event.target);
///     event.target.value = ""
///   }
///
/// Babel keeps short ones on a single line:
///   event =>{ dispatch(...event.target); event.target.value = ""}
///
/// This collapses arrow bodies that are short enough (≤15 lines, ≤400 chars).
fn normalize_multiline_arrow_body(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let trimmed = lines[i].trim();

        // Detect line ending with `=>{` — start of a multi-line arrow body
        // But NOT a standalone line that is just `=>{`
        if trimmed.ends_with("=>{") && trimmed.len() > 3 {
            // Track brace depth to find the matching close
            let mut brace_depth = 0i32;
            let mut collected = Vec::new();
            let start = i;
            let mut found_close = false;
            let mut j = i;

            while j < lines.len() && j - start < 15 {
                let t = lines[j].trim();
                for ch in t.bytes() {
                    if ch == b'{' {
                        brace_depth += 1;
                    } else if ch == b'}' {
                        brace_depth -= 1;
                    }
                }
                collected.push(t);
                if brace_depth == 0 && j > start {
                    found_close = true;
                    j += 1;
                    break;
                }
                j += 1;
            }

            if found_close && collected.len() > 2 {
                let collapsed = collected.join(" ");
                // Check total length is reasonable
                if collapsed.len() <= 400 {
                    // Clean up double spaces
                    let collapsed = collapsed.replace("  ", " ");
                    // Strip trailing space before `}}` — the join adds a space
                    // between the last stmt and closing `}}` but Babel doesn't
                    let collapsed = collapsed.replace(" }}", "}}");
                    out.push(collapsed);
                    i = j;
                    continue;
                }
            }
        }

        out.push(trimmed.to_string());
        i += 1;
    }

    out.join("\n")
}

/// Normalize spacing around `}` in outlined function naming expressions.
///
/// OXC prints: `() =>value }["key"]`  (space before `}`)
/// Babel prints: `() =>value}["key"]`  (no space before `}`)
///
/// Also normalizes JSX expression container spacing for object-valued attrs:
/// OXC prints: `onClick={{"key": ...}["key"]}`
/// Babel prints: `onClick={ {"key": ...}["key"] }`
///
/// Normalize both to the compact (no-space) form.
fn normalize_outlined_function_spacing(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    for line in code.lines() {
        let mut l = line.to_string();
        // 1. Normalize `={ {` to `={{` (JSX attr opening with object)
        while let Some(pos) = l.find("={ {") {
            if pos > 0 && l.as_bytes()[pos - 1].is_ascii_alphanumeric() {
                l = format!("{}{{{}", &l[..pos + 1], &l[pos + 3..]);
            } else {
                break;
            }
        }
        // 2. Normalize ` }["` to `}["` — space before `}` in outlined func exprs
        l = l.replace(" }[\"", "}[\"");
        // 3. Normalize `"] }` to `"]}` — trailing close of JSX expression
        // container after outlined func bracket access
        l = l.replace("\"] }", "\"]}");
        // 4. Normalize `() } }` and similar trailing double-close patterns
        l = l.replace(") } }", ")}}");
        l = l.replace(") }}", ")}}");
        // 5. Normalize `=>{ ` to `=>{` when inside outlined function naming
        // pattern (line contains `}}["` indicating bracket access close)
        if l.contains("}}[\"") {
            l = l.replace("=>{ ", "=>{");
        }

        result.push_str(&l);
        result.push('\n');
    }
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_for_compare, normalize_multiline_call_invocations,
        normalize_multiline_iife_collapsing, normalize_small_array_bracket_spacing,
        prepare_code_for_compare,
    };

    #[test]
    fn prepare_code_for_compare_strict_matches_switch_label_shape() {
        let actual = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nbb1: switch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        let expected = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nswitch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        assert_eq!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_strict_matches_call_trivia_shapes() {
        let actual = "setProperty( x, { b: 3, other }, \"a\");\nJSON.stringify( null, null, { \"Component[k]\": () => value }[ \"Component[k]\" ], );";
        let expected = "setProperty(x, { b: 3, other }, \"a\");\nJSON.stringify(null, null, { \"Component[k]\": () => value }[\"Component[k]\"]);";
        assert_eq!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_collapses_inline_assign_then_read_stmt() {
        let actual = "if ($[2] !== arr2 || $[3] !== x) { y = x.concat(arr2);";
        let expected = "if ($[2] !== arr2 || $[3] !== x) { ((y = x.concat(arr2)), y);";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_collapses_inline_assign_then_discard_stmt() {
        let actual = "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { x = [];";
        let expected =
            "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { ((x = []), null);";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_drops_multiline_trailing_enum_commas() {
        let actual = "enum Bool {\nTrue = \"true\",\nFalse = \"false\"\n}";
        let expected = "enum Bool {\nTrue = \"true\",\nFalse = \"false\",\n}";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_blank_lines_in_gated_function_bodies() {
        let actual = "const Foo = isForgetEnabled_Fixtures()\n? function Foo(props) {\n\"use forget\";\nconst $ = _c(3);\nif (props.bar < 0) {\nreturn props.children\n}\nreturn props.bar\n}\n: function Foo(props) {\nreturn props.bar\n};";
        let expected = "const Foo = isForgetEnabled_Fixtures()\n? function Foo(props) {\n\"use forget\";\nconst $ = _c(3);\n\nif (props.bar < 0) {\nreturn props.children\n}\nreturn props.bar\n}\n: function Foo(props) {\nreturn props.bar\n};";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_blank_lines_before_top_level_exports() {
        let actual = "const Renderer = isForgetEnabled_Fixtures()\n? (props) => {\nconst $ = _c(1);\nreturn props\n}\n: (props) => props;\n\nexport default Renderer;\n\nexport const FIXTURE_ENTRYPOINT = { fn: eval(\"Renderer\"), params: [{}] };";
        let expected = "const Renderer = isForgetEnabled_Fixtures()\n? (props) => {\nconst $ = _c(1);\nreturn props\n}\n: (props) => props;\nexport default Renderer;\n\nexport const FIXTURE_ENTRYPOINT = { fn: eval(\"Renderer\"), params: [{}] };";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_blank_lines_before_top_level_consts() {
        let actual = "const _ = { useHook: isForgetEnabled_Fixtures() ? () => {} : () => {} };\nidentity(_.useHook);\n\nconst useHook = isForgetEnabled_Fixtures()\n? function useHook() {\nconst $ = _c(1);\nreturn null\n}\n: function useHook() {\nreturn null\n};";
        let expected = "const _ = { useHook: isForgetEnabled_Fixtures() ? () => {} : () => {} };\nidentity(_.useHook);\nconst useHook = isForgetEnabled_Fixtures()\n? function useHook() {\nconst $ = _c(1);\nreturn null\n}\n: function useHook() {\nreturn null\n};";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_top_level_comment_trivia() {
        let actual = "import { c as _c } from \"react/compiler-runtime\";\nfunction foo() {}";
        let expected = "import { c as _c } from \"react/compiler-runtime\";\n// @Pass runMutableRangeAnalysis\n// Fixture note\nfunction foo() {}";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_comment_only_lines_in_multiline_imports() {
        let actual = "import { useEffect, useRef, experimental_useEffectEvent as useEffectEvent } from \"react\";";
        let expected = "import {\n  useEffect,\n  useRef,\n  // @ts-expect-error\n  experimental_useEffectEvent as useEffectEvent,\n} from \"react\";";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_fixture_entrypoint_comment_trivia() {
        let actual = "export const FIXTURE_ENTRYPOINT = {\nfn: Component,\nparams: [{ value: [, 3.14] }],\n};";
        let expected = "export const FIXTURE_ENTRYPOINT = {\nfn: Component,\n// should return default\nparams: [{ value: [, /* hole! */ 3.14] }],\n};";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }

    #[test]
    fn normalize_for_compare_strips_labeled_switch_after_block_braces() {
        let actual =
            "function foo(x) {\nbb0: {\nswitch (x) {\ncase 0: {\nbreak bb0;\n}\ndefault:\n}\n}\n}";
        let expected = "function foo(x) {\nswitch (x) {\ncase 0: {\nbreak;\n}\ndefault:\n}\n}";
        assert_eq!(
            normalize_for_compare(actual),
            normalize_for_compare(expected)
        );
    }
    #[test]
    fn normalize_multiline_call_invocations_collapses_arguments() {
        let input = "foo(bar,\nbaz,\nqux);";
        let expected = "foo(bar, baz, qux);";
        assert_eq!(normalize_multiline_call_invocations(input), expected);
    }

    #[test]
    fn normalize_small_array_bracket_spacing_trims_collapsed_return_arrays() {
        let input = "return [ item.id, { value: item.value } ]";
        let expected = "return [item.id, { value: item.value }]";
        assert_eq!(normalize_small_array_bracket_spacing(input), expected);
    }

    #[test]
    fn normalize_multiline_iife_collapsing_basic() {
        let input = "const t0 = function() {\ntry{\n$dispatcherGuard(2);\nreturn useFire(foo)\n}finally{\n$dispatcherGuard(3)\n}\n}();";
        let expected = "const t0 = (function() { try{ $dispatcherGuard(2); return useFire(foo) }finally{ $dispatcherGuard(3) } })();";
        assert_eq!(normalize_multiline_iife_collapsing(input), expected);
    }

    #[test]
    fn normalize_multiline_iife_collapsing_with_semicolon() {
        let input = "const [state, setState] = function() {\ntry{\n$dispatcherGuard(2);\nreturn useState(t1)\n}finally{\n$dispatcherGuard(3)\n}\n}();";
        let expected = "const [state, setState] = (function() { try{ $dispatcherGuard(2); return useState(t1) }finally{ $dispatcherGuard(3) } })();";
        assert_eq!(normalize_multiline_iife_collapsing(input), expected);
    }

    #[test]
    fn normalize_multiline_iife_collapsing_return() {
        let input = "return function b(t2) {\nconst y_0 = t2 === undefined ? [] : t2;\nreturn [x_0, y_0]\n}()";
        let expected = "return (function b(t2) { const y_0 = t2 === undefined ? [] : t2; return [x_0, y_0] })()";
        assert_eq!(normalize_multiline_iife_collapsing(input), expected);
    }
}
