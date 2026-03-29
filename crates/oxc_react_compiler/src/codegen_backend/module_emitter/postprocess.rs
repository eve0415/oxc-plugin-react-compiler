use oxc_allocator::Allocator;
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{SPAN, SourceType};
use oxc_syntax::identifier::is_identifier_name;

use super::{InsertedImport, InsertedImportSpec};

pub(super) fn build_inserted_import_statement<'a>(
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

pub(super) fn build_runtime_import_merge_statement<'a>(
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

pub(super) fn parse_statements<'a>(
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

pub(super) fn codegen_program(program: &ast::Program<'_>) -> String {
    codegen_program_with_source_map(program, None).0
}

pub(super) fn codegen_program_with_source_map(
    program: &ast::Program<'_>,
    source_map_path: Option<&str>,
) -> (String, Option<oxc_sourcemap::SourceMap>) {
    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        source_map_path: source_map_path.map(std::path::PathBuf::from),
        ..CodegenOptions::default()
    };
    let result = Codegen::new().with_options(options).build(program);
    (result.code, result.map)
}

pub(super) fn compact_simple_jsx_object_attributes(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let mut cursor = 0usize;

    while let Some(relative_eq) = code[cursor..].find("={{") {
        let eq_index = cursor + relative_eq;
        result.push_str(&code[cursor..eq_index]);

        let mut depth = 0usize;
        let mut end_index: Option<usize> = None;
        for (offset, ch) in code[eq_index + 1..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end_index = Some(eq_index + 1 + offset);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end_index) = end_index else {
            result.push_str(&code[eq_index..]);
            return result;
        };

        let object_expr = &code[eq_index + 2..end_index];
        let inner = &object_expr[1..object_expr.len().saturating_sub(1)];
        if object_expr.contains('\n') && !inner.contains('{') && !inner.contains('}') {
            result.push('=');
            result.push('{');
            result.push_str(&compact_single_statement(object_expr));
            result.push('}');
        } else {
            result.push_str(&code[eq_index..=end_index]);
        }
        cursor = end_index + 1;
    }

    result.push_str(&code[cursor..]);
    result
}

pub(super) fn compact_single_statement(code: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() && !prev_space {
            out.push(' ');
        }
        out.push_str(trimmed);
        prev_space = out.ends_with(' ');
    }
    out
}

/// Move leading file comment(s) to be trailing on the last import line in the import block.
///
/// Babel's codegen places leading file comments (e.g., pragmas) as trailing comments
/// on the last import statement in the import block. Our AST codegen emits them on
/// separate lines after the imports. This function detects when the last import is
/// followed by a line comment and moves it to be trailing on the import line.
///
/// Pattern: `import ...;\nimport ...;\n// comment\n` → `import ...;\nimport ...; // comment\n`
// Fix OXC's trailing space before `]` in single-line arrays.
// OXC emits `[0, 1, 2 ]` where Babel emits `[0, 1, 2]`.
pub(super) fn fix_oxc_array_trailing_space(code: &str) -> String {
    if !code.contains(" ]") {
        return code.to_string();
    }
    let mut result = String::with_capacity(code.len());
    for line in code.split('\n') {
        let mut line_bytes: Vec<u8> = line.as_bytes().to_vec();
        let mut changed = true;
        while changed {
            changed = false;
            let len = line_bytes.len();
            for i in 1..len {
                if line_bytes[i] == b']' && line_bytes[i - 1] == b' ' {
                    let mut depth: usize = 1;
                    let mut j = i.wrapping_sub(2);
                    let mut found_open = false;
                    while j < len {
                        match line_bytes[j] {
                            b']' => depth += 1,
                            b'[' => {
                                depth -= 1;
                                if depth == 0 {
                                    found_open = true;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j = j.wrapping_sub(1);
                    }
                    if found_open && j + 1 < len && line_bytes[j + 1] != b' ' {
                        line_bytes.remove(i - 1);
                        changed = true;
                        break;
                    }
                }
            }
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&String::from_utf8_lossy(&line_bytes));
    }
    result
}

/// Fix OXC formatting of gating ternary expressions with function-expression or
/// arrow-function branches.
///
/// Babel formats `test() ? function F(...) { ... } : function F(...) { ... }` and
/// `test() ? (p) => { ... } : (p) => expr` with line breaks before `?` and `:`,
/// while OXC puts everything on one line:
///   `test() ? function F(` → `test()\n? function F(`
///   `} : function F(`     → `}\n: function F(`
///   `test() ? (p) =>{`    → `test()\n? (p) =>{`
///   `} : (p) =>expr`      → `}\n: (p) =>expr`
pub(super) fn fix_gating_ternary_line_breaks(code: &str) -> String {
    // Only apply when the code contains a gating ternary.
    if !code.contains("() ? function ") && !code.contains("() ? (") && !code.contains("() ? ") {
        return code.to_string();
    }
    let mut result = String::with_capacity(code.len() + 16);
    for line in code.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        // Pattern: `... test() ? function Name(` on a single line
        if let Some(pos) = line.find("() ? function ") {
            // Insert line break: `test()` then newline then `? function ...`
            result.push_str(&line[..pos + 2]); // up to and including "()"
            result.push('\n');
            result.push_str(&line[pos + 3..]); // skip the space, keep "? function ..."
        } else if let Some(pos) = line.find("} : function ") {
            // Pattern: `} : function Name(` → `}\n: function Name(`
            result.push_str(&line[..pos + 1]); // up to and including "}"
            result.push('\n');
            result.push_str(&line[pos + 2..]); // skip the space, keep ": function ..."
        }
        // Arrow function patterns for gating ternaries
        else if is_gating_arrow_ternary_start(line) {
            // Pattern: `... test() ? (params) =>{` or `... test() ? params =>{`
            let pos = line.find("() ? ").unwrap();
            result.push_str(&line[..pos + 2]); // up to and including "()"
            result.push('\n');
            result.push_str(&line[pos + 3..]); // skip the space, keep "? ..."
        } else if is_gating_arrow_alternate(line) {
            // Pattern: `} : (params) =>...` or `} : params =>...`
            let pos = line.find("} : ").unwrap();
            result.push_str(&line[..pos + 1]); // up to and including "}"
            result.push('\n');
            result.push_str(&line[pos + 2..]); // skip the space, keep ": ..."
        } else {
            result.push_str(line);
        }
    }
    result
}

/// Check if a line contains a gating ternary start with arrow function consequent.
/// Matches: `= isForgetEnabled_Fixtures() ? (params) =>{` or `() ? params =>{`
fn is_gating_arrow_ternary_start(line: &str) -> bool {
    let Some(pos) = line.find("() ? ") else {
        return false;
    };
    // Already handled by function pattern
    if line[pos..].starts_with("() ? function ") {
        return false;
    }
    // The rest after `() ? ` should start an arrow: `(params) =>{` or `ident =>{`
    let after = &line[pos + 5..]; // skip "() ? "
    contains_arrow_start(after)
}

/// Check if a line contains a gating ternary alternate with arrow function.
/// Matches: `} : (params) =>...` or `} : params =>...`
fn is_gating_arrow_alternate(line: &str) -> bool {
    let Some(pos) = line.find("} : ") else {
        return false;
    };
    // Already handled by function pattern
    if line[pos..].starts_with("} : function ") {
        return false;
    }
    let after = &line[pos + 4..]; // skip "} : "
    contains_arrow_start(after)
}

/// Check if text starts with an arrow function pattern:
/// `(params) =>` or `ident =>` (with arrow appearing somewhere in the text).
fn contains_arrow_start(text: &str) -> bool {
    text.contains("=>")
}

/// Collapse multiline JSX in gating ternary fallback arrow expressions.
///
/// OXC reprints the fallback (unoptimized) arrow's JSX body across multiple lines,
/// while Babel keeps it on a single line. After `fix_gating_ternary_line_breaks`,
/// the pattern is:
///   `: params =><Tag>\n<Child></Child>\n</Tag>;`
/// This collapses it to:
///   `: params =><Tag><Child></Child></Tag>;`
pub(super) fn fix_gating_ternary_fallback_arrow_jsx(code: &str) -> String {
    // Quick bail: only applies when we have a ternary alternate with arrow + JSX
    if !code.contains(": ") || !code.contains("=>") {
        return code.to_string();
    }
    let lines: Vec<&str> = code.split('\n').collect();
    let mut result = String::with_capacity(code.len());
    let mut i = 0;
    while i < lines.len() {
        if !result.is_empty() {
            result.push('\n');
        }
        let trimmed = lines[i].trim();
        // Detect `: params =><Tag>` where next lines are JSX children/close
        if trimmed.starts_with(": ") && trimmed.contains("=><") && !trimmed.ends_with(';') {
            // Check if this starts a multiline JSX block in a ternary alternate
            // Collect lines until we find the closing tag + semicolon
            let mut collected = String::from(trimmed);
            let start_i = i;
            i += 1;
            let mut found_end = false;
            while i < lines.len() {
                let next_trimmed = lines[i].trim();
                if next_trimmed.is_empty() {
                    break;
                }
                collected.push_str(next_trimmed);
                if next_trimmed.ends_with(';') || next_trimmed.ends_with(");") {
                    found_end = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if found_end {
                result.push_str(&collected);
            } else {
                // Didn't find a proper end, emit lines as-is
                result.push_str(lines[start_i]);
                // Re-emit the lines we consumed
                for line in &lines[(start_i + 1)..i] {
                    result.push('\n');
                    result.push_str(line);
                }
            }
            continue;
        }
        result.push_str(lines[i]);
        i += 1;
    }
    result
}

/// Fix OXC formatting of unoptimized function parameter wrapping.
///
/// Babel wraps long parameter lists in `_unoptimized` function declarations (which
/// retain Flow/TS type annotations) across multiple lines. OXC puts them on one line.
/// E.g. `function F_unoptimized(p1: T1, p2: T2): R{` becomes:
///       `function F_unoptimized(p1: T1,\np2: T2): R{`
pub(super) fn fix_unoptimized_function_param_wrapping(code: &str) -> String {
    if !code.contains("_unoptimized(") {
        return code.to_string();
    }
    let mut result = String::with_capacity(code.len() + 16);
    for line in code.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        // Only target lines that declare an `_unoptimized` function with typed params
        if line.contains("_unoptimized(") && line.starts_with("function ") {
            // Find the opening paren of the params
            if let Some(paren_start) = line.find('(') {
                // Extract the params section: from `(` to the matching `)`
                let after_paren = &line[paren_start + 1..];
                if let Some(paren_end_rel) = find_matching_close_paren(after_paren) {
                    let params_str = &after_paren[..paren_end_rel];
                    // Only wrap if there are typed params (contains `:`) and
                    // more than one param (contains `, `)
                    if params_str.contains(':') && params_str.contains(", ") {
                        // Split at top-level `, ` boundaries (not inside generics)
                        let wrapped = wrap_params_at_commas(params_str);
                        result.push_str(&line[..paren_start + 1]);
                        result.push_str(&wrapped);
                        result.push_str(&line[paren_start + 1 + paren_end_rel..]);
                        continue;
                    }
                }
            }
        }
        result.push_str(line);
    }
    result
}

/// Wrap JSX in assignment expressions with parentheses to match Babel's printer behavior.
///
/// OXC's codegen strips `ParenthesizedExpression` nodes, so our AST-level
/// `maybe_parenthesize_jsx` has no effect. This post-processing pass re-adds
/// `( ... )` around JSX in assignments where Babel would output multi-line JSX
/// (which it wraps in parentheses).
///
/// Babel outputs multi-line (parenthesized) JSX when the element is not self-closing
/// AND has any of:
///   - Attributes on the opening tag
///   - Child JSX elements (nested `<` inside)
///   - Multiple expression children (`{...}{...}`)
///
/// Simple cases like `t0 = <div>{bool}</div>` (single expression child, no attrs)
/// are left unwrapped.
pub(super) fn fix_jsx_assignment_parens(code: &str) -> String {
    // Quick bail: if no JSX assignments exist, return early
    if !code.contains("= <") {
        return code.to_string();
    }
    let mut result = String::with_capacity(code.len() + 64);
    for line in code.split('\n') {
        if !result.is_empty() {
            result.push('\n');
        }
        if let Some(fixed) = try_wrap_jsx_assignment_parens(line) {
            result.push_str(&fixed);
        } else {
            result.push_str(line);
        }
    }
    result
}

/// Try to wrap JSX in an assignment on a single line with `( ... )`.
/// Returns `Some(wrapped_line)` if the line matches the pattern and the JSX
/// is complex enough to need wrapping, `None` otherwise.
fn try_wrap_jsx_assignment_parens(line: &str) -> Option<String> {
    let trimmed = line.trim();

    // Match pattern: `IDENT = <...>;`
    // The identifier can be a temp like `t0`, `t1`, or any identifier.
    let eq_jsx_pos = trimmed.find("= <")?;

    // Verify the left side is a simple identifier (word chars before `= `)
    let before_eq = trimmed[..eq_jsx_pos].trim_end();
    if before_eq.is_empty()
        || !before_eq
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return None;
    }

    // The JSX part starts after `= ` and ends before `;`
    if !trimmed.ends_with(';') {
        return None;
    }
    let jsx_part = &trimmed[eq_jsx_pos + 2..trimmed.len() - 1]; // skip "= " and trailing ";"

    // Must start with `<`
    if !jsx_part.starts_with('<') {
        return None;
    }

    // Don't wrap self-closing elements like `<Component prop={val} />`
    if jsx_part.ends_with("/>") {
        return None;
    }

    // Check if this JSX needs parenthesization (would be multi-line in Babel)
    if !jsx_needs_parens(jsx_part) {
        return None;
    }

    // Build the wrapped line preserving leading whitespace
    let leading_ws = &line[..line.len() - line.trim_start().len()];
    Some(format!("{}{} = ( {} );", leading_ws, before_eq, jsx_part))
}

/// Determine whether JSX content would be multi-line in Babel's output,
/// thus requiring parenthesization.
///
/// Returns true if the JSX has:
/// 1. Attributes on the opening tag, OR
/// 2. Child JSX elements (nested `<` after the opening tag's `>`), OR
/// 3. Multiple expression children (`{...}` appearing 2+ times)
fn jsx_needs_parens(jsx: &str) -> bool {
    let bytes = jsx.as_bytes();
    if bytes.is_empty() || bytes[0] != b'<' {
        return false;
    }

    // Find the end of the opening tag (the first `>` that closes it)
    // We need to handle `<Tag attr={expr}>` where `>` appears inside `{...}`
    let mut brace_depth = 0u32;
    let mut opening_tag_end = None;
    let mut has_attrs = false;
    let mut past_tag_name = false;

    for (i, &b) in bytes.iter().enumerate().skip(1) {
        match b {
            b'{' => brace_depth += 1,
            b'}' if brace_depth > 0 => brace_depth -= 1,
            b' ' | b'\t' if brace_depth == 0 && !past_tag_name => {
                past_tag_name = true;
            }
            b'>' if brace_depth == 0 => {
                // Check if this is `/>` (self-closing)
                if i > 0 && bytes[i - 1] == b'/' {
                    return false; // self-closing, no wrapping needed
                }
                if past_tag_name {
                    // There's content between tag name and `>` → may have attributes
                    let between = &jsx[1..i];
                    let tag_and_rest = between.trim();
                    if !tag_and_rest.is_empty() {
                        let tag_name_end = tag_and_rest
                            .find(|c: char| c.is_whitespace())
                            .unwrap_or(tag_and_rest.len());
                        if tag_name_end < tag_and_rest.len() {
                            has_attrs = true;
                        }
                    }
                }
                opening_tag_end = Some(i);
                break;
            }
            _ => {
                if brace_depth == 0 && past_tag_name && !b.is_ascii_whitespace() {
                    has_attrs = true;
                }
            }
        }
    }

    let opening_end = match opening_tag_end {
        Some(pos) => pos,
        None => return false, // malformed
    };

    // Condition 1: has attributes
    if has_attrs {
        return true;
    }

    // Look at the content between opening tag end and closing tag
    // Find the closing tag: last occurrence of `</`
    let children_start = opening_end + 1;
    let closing_tag_start = match jsx.rfind("</") {
        Some(pos) if pos >= children_start => pos,
        _ => return false,
    };

    let children = &jsx[children_start..closing_tag_start];
    if children.is_empty() {
        return false;
    }

    // Condition 2: has child JSX elements (nested `<` that's not inside `{...}`)
    let mut child_brace_depth = 0u32;
    let mut expr_child_count = 0u32;
    let child_bytes = children.as_bytes();
    for &b in child_bytes {
        match b {
            b'{' => {
                if child_brace_depth == 0 {
                    expr_child_count += 1;
                }
                child_brace_depth += 1;
            }
            b'}' if child_brace_depth > 0 => child_brace_depth -= 1,
            b'<' if child_brace_depth == 0 => {
                // Found a child JSX element
                return true;
            }
            _ => {}
        }
    }

    // Condition 3: multiple expression children
    if expr_child_count >= 2 {
        return true;
    }

    false
}

/// Find the position of the matching `)` for a string starting after `(`.
/// Handles nested parens and angle brackets.
fn find_matching_close_paren(s: &str) -> Option<usize> {
    let mut depth = 1u32;
    let mut angle_depth = 0u32;
    for (i, ch) in s.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' if angle_depth > 0 => angle_depth -= 1,
            '(' if angle_depth == 0 => depth += 1,
            ')' if angle_depth == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Wrap parameters at top-level comma boundaries.
/// Replaces `, ` with `,\n` at the top level (not inside `<>` or `()`).
pub(super) fn wrap_params_at_commas(params: &str) -> String {
    let mut result = String::with_capacity(params.len() + 8);
    let mut paren_depth = 0u32;
    let mut angle_depth = 0u32;
    let bytes = params.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        let ch = bytes[i];
        match ch {
            b'<' => {
                angle_depth += 1;
                result.push('<');
            }
            b'>' if angle_depth > 0 => {
                angle_depth -= 1;
                result.push('>');
            }
            b'(' => {
                paren_depth += 1;
                result.push('(');
            }
            b')' if paren_depth > 0 => {
                paren_depth -= 1;
                result.push(')');
            }
            b',' if paren_depth == 0 && angle_depth == 0 => {
                // At top-level comma: replace `, ` with `,\n`
                result.push(',');
                if i + 1 < len && bytes[i + 1] == b' ' {
                    result.push('\n');
                    i += 1; // skip the space
                }
            }
            _ => {
                result.push(ch as char);
            }
        }
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use crate::test_utils::compile_to_result;

    #[test]
    fn postprocess_no_crash() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        assert!(!result.code.is_empty(), "output should be non-empty");
    }

    #[test]
    fn postprocess_round_trip() {
        let result = compile_to_result(
            "function Component(props) { const x = props.a + 1; return <div>{x}</div>; }",
        );
        assert!(result.transformed, "should be transformed");
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs().with_jsx(true);
        let parsed = oxc_parser::Parser::new(&allocator, &result.code, source_type).parse();
        assert!(
            parsed.errors.is_empty(),
            "re-parse failed: {:?}",
            parsed.errors
        );
    }

    #[test]
    fn sourcemap_is_generated() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        assert!(result.transformed, "should be transformed");
        assert!(
            result.map.is_some(),
            "sourcemap should be generated for transformed code"
        );
    }

    #[test]
    fn sourcemap_is_valid_json() {
        let result = compile_to_result(
            "function Component(props) { const x = props.a + 1; return <div>{x}</div>; }",
        );
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(map_json).expect("sourcemap should be valid JSON");
        assert_eq!(parsed["version"], 3, "sourcemap version should be 3");
        assert!(
            parsed["mappings"].is_string(),
            "sourcemap should have mappings field"
        );
        assert!(
            parsed["sources"].is_array(),
            "sourcemap should have sources array"
        );
    }

    #[test]
    fn sourcemap_contains_source_content() {
        let source = "function Component(props) { return <div>{props.x}</div>; }";
        let result = compile_to_result(source);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(map_json).expect("sourcemap should be valid JSON");
        let sources_content = parsed["sourcesContent"]
            .as_array()
            .expect("should have sourcesContent");
        assert!(
            !sources_content.is_empty(),
            "sourcesContent should not be empty"
        );
        assert_eq!(
            sources_content[0].as_str().unwrap(),
            source,
            "sourcesContent should contain the original source"
        );
    }

    #[test]
    fn sourcemap_has_nonempty_mappings() {
        let result = compile_to_result(
            "function Component(props) { const x = props.a + 1; return <div>{x}</div>; }",
        );
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value = serde_json::from_str(map_json).unwrap();
        let mappings = parsed["mappings"].as_str().unwrap();
        assert!(
            !mappings.is_empty(),
            "sourcemap mappings should not be empty"
        );
    }

    #[test]
    fn no_sourcemap_when_not_transformed() {
        // A function that doesn't look like a component shouldn't be transformed.
        let result = compile_to_result("function helper(x) { return x + 1; }");
        assert!(!result.transformed);
        assert!(result.map.is_none(), "no sourcemap for untransformed code");
    }
}
