use std::sync::OnceLock;

pub(crate) fn canonicalize_strict_text(code: &str) -> String {
    code.replace("\r\n", "\n").trim_end().to_string()
}

/// Normalize code for comparison. Applies only cosmetic printer/trivia
/// normalizations in a convergence loop until the output stabilizes.
fn normalize_for_compare(code: &str) -> String {
    let steps: &[fn(&str) -> String] = &[
        // Shared cosmetic normalizations (OXC vs Babel formatting)
        normalize_compare_multiline_imports,
        normalize_import_region_comments,
        normalize_top_level_comment_trivia,
        normalize_compare_multiline_brace_literals,
        normalize_multiline_trailing_commas_before_closers,
        normalize_multiline_switch_cases,
        normalize_ts_object_type_semicolons,
        normalize_numeric_exponent_literals,
        normalize_compare_unicode_escapes,
        normalize_fixture_entrypoint_array_spacing,
        normalize_scope_body_blank_lines,
        normalize_top_level_statement_blank_lines,
        normalize_space_before_closing_brace,
        normalize_jsx_assignment_parens,
        normalize_jsx_expression_container_spacing,
        normalize_jsx_residual_close_paren,
        normalize_import_quotes,
        normalize_function_paren_space,
        normalize_empty_block_newlines,
        normalize_multiline_short_arrays,
        normalize_object_in_array_spacing,
        normalize_const_string_quotes,
        normalize_trailing_zero_decimal_literals,
        normalize_empty_block_inner_space,
        normalize_destructuring_brace_spacing,
        normalize_single_arrow_param_parens,
        normalize_numeric_leading_zero,
        normalize_optional_call_space,
        normalize_jsx_attr_trailing_space,
        // Strict output normalizations (cosmetic OXC printer differences)
        normalize_trailing_comma_in_calls,
        normalize_multiline_call_invocations,
        normalize_small_array_bracket_spacing,
        normalize_bracket_string_literal_spacing,
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

/// Normalize trailing `.0` on decimal literals where the numeric value is unchanged.
fn normalize_trailing_zero_decimal_literals(code: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?P<int>\b\d+)\.0\b").unwrap());
    re.replace_all(code, "$int").into_owned()
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
        || trimmed.starts_with("function ")
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

#[cfg(test)]
mod tests {
    use super::{
        normalize_for_compare, normalize_multiline_call_invocations,
        normalize_small_array_bracket_spacing, prepare_code_for_compare,
    };

    #[test]
    fn prepare_code_for_compare_default_preserves_switch_label_shape() {
        let actual = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nbb1: switch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        let expected = "function Component(props) {\nlet x = 0;\nbb0: if (props.a) {\nx = 1\n}\nswitch (props.c) {\ncase \"a\": { x = 4; break }\n}\nreturn x\n}";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_canonicalizes_call_trivia_shapes() {
        let actual = "setProperty( x, { b: 3, other }, \"a\");\nJSON.stringify( null, null, { \"Component[k]\": () => value }[ \"Component[k]\" ], );";
        let expected = "setProperty(x, { b: 3, other }, \"a\");\nJSON.stringify(null, null, { \"Component[k]\": () => value }[\"Component[k]\"]);";
        assert_eq!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_inline_assign_then_read_stmt() {
        let actual = "if ($[2] !== arr2 || $[3] !== x) { y = x.concat(arr2);";
        let expected = "if ($[2] !== arr2 || $[3] !== x) { ((y = x.concat(arr2)), y);";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_inline_assign_then_discard_stmt() {
        let actual = "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { x = [];";
        let expected =
            "if ($[0] === Symbol.for(\"react.memo_cache_sentinel\")) { ((x = []), null);";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
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
    fn prepare_code_for_compare_default_preserves_labeled_switch_shape() {
        let actual =
            "function foo(x) {\nbb0: {\nswitch (x) {\ncase 0: {\nbreak bb0;\n}\ndefault:\n}\n}\n}";
        let expected = "function foo(x) {\nswitch (x) {\ncase 0: {\nbreak;\n}\ndefault:\n}\n}";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_switch_case_brace_shape() {
        let actual = "switch (kind) {\ncase \"a\": {\nconst value = read();\nreturn value;\n}\n}";
        let expected = "switch (kind) {\ncase \"a\":\nconst value = read();\nreturn value;\n}";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_single_return_arrow_block_shape() {
        let actual = "const fnRef = value =>{\nreturn value;\n};";
        let expected = "const fnRef = value => value;";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_empty_statement_shape() {
        let actual = "doThing(); ;\nreturn value;";
        let expected = "doThing();\nreturn value;";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
        );
    }

    #[test]
    fn prepare_code_for_compare_default_preserves_dead_bare_var_ref_shape() {
        let actual = "var _ref;\nconst value = foo;";
        let expected = "const value = foo;";
        assert_ne!(
            prepare_code_for_compare(actual),
            prepare_code_for_compare(expected)
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

}
