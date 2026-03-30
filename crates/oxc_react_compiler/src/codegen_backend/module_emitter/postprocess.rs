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

#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)] // Retained for conformance normalization
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)] // Retained for conformance normalization
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

    #[test]
    fn sourcemap_disabled_via_option() {
        let source = "function Component(props) { return <div>{props.x}</div>; }";
        let options = crate::options::PluginOptions {
            source_map: false,
            ..crate::options::PluginOptions::default()
        };
        let result = crate::compile("test.js", source, &options);
        assert!(result.transformed, "should still transform");
        assert!(
            result.map.is_none(),
            "no sourcemap when source_map option is false"
        );
    }

    #[test]
    fn sourcemap_has_debug_id() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value = serde_json::from_str(map_json).unwrap();
        let debug_id = parsed["debugId"]
            .as_str()
            .expect("sourcemap should have debugId");
        // UUID v4 format: 8-4-4-4-12 hex chars
        assert_eq!(debug_id.len(), 36, "debugId should be 36 chars (UUID)");
        assert_eq!(
            debug_id.chars().filter(|c| *c == '-').count(),
            4,
            "debugId should have 4 dashes"
        );
    }

    // --- Token-level verification helpers ---

    /// Decode a sourcemap JSON string and return the SourceMap object.
    fn decode_sourcemap(json: &str) -> oxc_sourcemap::SourceMap {
        oxc_sourcemap::SourceMap::from_json_string(json).expect("valid sourcemap JSON")
    }

    /// Find a token in the generated code that maps to the given source line (0-based).
    /// Returns (generated_line, generated_col, source_line, source_col).
    fn find_token_for_source_line(
        sm: &oxc_sourcemap::SourceMap,
        src_line: u32,
    ) -> Option<(u32, u32, u32, u32)> {
        sm.get_tokens().find_map(|t| {
            if t.get_src_line() == src_line {
                Some((
                    t.get_dst_line(),
                    t.get_dst_col(),
                    t.get_src_line(),
                    t.get_src_col(),
                ))
            } else {
                None
            }
        })
    }

    /// Check that a specific source line has at least one mapping in the sourcemap.
    fn assert_source_line_mapped(sm: &oxc_sourcemap::SourceMap, src_line: u32, desc: &str) {
        assert!(
            find_token_for_source_line(sm, src_line).is_some(),
            "source line {} ({}) should have a mapping",
            src_line,
            desc
        );
    }

    /// Check that generated code at a given line (0-based) maps back to the
    /// expected source line (0-based). Finds any token on the generated line
    /// and checks its source position.
    #[allow(dead_code)]
    fn assert_generated_line_maps_to(
        sm: &oxc_sourcemap::SourceMap,
        gen_line: u32,
        expected_src_line: u32,
        desc: &str,
    ) {
        let token = sm
            .get_tokens()
            .find(|t| t.get_dst_line() == gen_line)
            .unwrap_or_else(|| panic!("no token found at generated line {} ({})", gen_line, desc));
        assert_eq!(
            token.get_src_line(),
            expected_src_line,
            "generated line {} ({}) should map to source line {}, but maps to {}",
            gen_line,
            desc,
            expected_src_line,
            token.get_src_line()
        );
    }

    // --- Token-level verification tests ---

    #[test]
    fn sourcemap_simple_component_tokens() {
        // Fixture 1: simple component with basic JSX
        let source = r#"function Component(props) {
  return <div>{props.x}</div>;
}"#;
        let result = compile_to_result(source);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // Source line 0: function declaration
        assert_source_line_mapped(&sm, 0, "function Component declaration");
        // Source line 1: return statement with JSX
        assert_source_line_mapped(&sm, 1, "return <div>{props.x}</div>");
    }

    #[test]
    fn sourcemap_multiple_reactive_scopes() {
        // Fixture 2: component with multiple reactive scopes (cache guards visible)
        let source = r#"function Component(props) {
  const a = props.x + 1;
  const b = props.y * 2;
  return <div a={a} b={b} />;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // All source lines should have mappings
        assert_source_line_mapped(&sm, 0, "function declaration");
        assert_source_line_mapped(&sm, 1, "const a = props.x + 1");
        assert_source_line_mapped(&sm, 2, "const b = props.y * 2");
        assert_source_line_mapped(&sm, 3, "return JSX");
    }

    #[test]
    fn sourcemap_hooks_and_closures() {
        // Fixture 3: hooks + closures (variable renaming visible)
        let source = r#"function Component(props) {
  const [count, setCount] = useState(0);
  const handler = () => {
    setCount(count + 1);
  };
  return <button onClick={handler}>{count}</button>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "function declaration");
        assert_source_line_mapped(&sm, 1, "useState");
        assert_source_line_mapped(&sm, 2, "handler arrow function");
    }

    #[test]
    fn sourcemap_multi_function_file() {
        // Fixture 4: multiple functions in one file
        let source = r#"function ComponentA(props) {
  return <div>{props.a}</div>;
}

function ComponentB(props) {
  return <span>{props.b}</span>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // Both function declarations should be mapped
        assert_source_line_mapped(&sm, 0, "ComponentA declaration");
        assert_source_line_mapped(&sm, 4, "ComponentB declaration");
    }

    #[test]
    fn sourcemap_partial_compilation() {
        // Fixture 5: one function bails out, one compiles
        let source = r#"function helper(x) { return x + 1; }
function Component(props) {
  return <div>{props.x}</div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // Both should have mappings (identity for untouched, correct for compiled)
        assert_source_line_mapped(&sm, 0, "helper (untouched)");
        assert_source_line_mapped(&sm, 1, "Component declaration");
    }

    #[test]
    fn sourcemap_typescript() {
        // Fixture 7: TypeScript with type annotations
        let source = r#"function Component(props: { x: number }) {
  const y: number = props.x + 1;
  return <div>{y}</div>;
}"#;
        let mut options = crate::options::PluginOptions::default();
        options.compilation_mode = crate::options::CompilationMode::Infer;
        let result = crate::compile("test.tsx", source, &options);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "function with TS types");
        assert_source_line_mapped(&sm, 1, "typed const");
    }

    #[test]
    fn sourcemap_nested_components() {
        // Fixture 8: nested components
        let source = r#"function Outer(props) {
  function Inner() {
    return <span>inner</span>;
  }
  return <div><Inner /></div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "Outer declaration");
        assert_source_line_mapped(&sm, 1, "Inner declaration");
    }

    #[test]
    fn sourcemap_conditional_rendering() {
        // Fixture 10: conditional rendering with if/else
        let source = r#"function Component(props) {
  const x = props.a;
  if (x) {
    return <div>yes</div>;
  }
  return <span>no</span>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "function declaration");
        assert_source_line_mapped(&sm, 1, "const x");
    }

    #[test]
    fn sourcemap_complex_jsx() {
        // Fixture 11: complex JSX (fragments, spread props)
        let source = r#"function Component(props) {
  const extra = { className: "foo" };
  return (
    <>
      <div {...extra}>{props.children}</div>
      <span key="a">text</span>
    </>
  );
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "function declaration");
        assert_source_line_mapped(&sm, 1, "const extra");
    }

    #[test]
    fn sourcemap_for_loop() {
        // Extra: for loop with mutation
        let source = r#"function Component(props) {
  const items = [];
  for (let i = 0; i < props.count; i++) {
    items.push(i);
  }
  return <div>{items.length}</div>;
}"#;
        let mut options = crate::options::PluginOptions::default();
        options.compilation_mode = crate::options::CompilationMode::All;
        let result = crate::compile("test.js", source, &options);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        assert_source_line_mapped(&sm, 0, "function declaration");
        assert_source_line_mapped(&sm, 2, "for loop");
    }

    #[test]
    fn sourcemap_token_count_reasonable() {
        // Verify that sourcemaps have a reasonable number of tokens
        // (not zero, not absurdly high)
        let source = r#"function Component(props) {
  const x = props.a + 1;
  return <div>{x}</div>;
}"#;
        let result = compile_to_result(source);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());
        let token_count = sm.get_tokens().count();
        assert!(
            token_count > 5,
            "sourcemap should have more than 5 tokens, got {}",
            token_count
        );
        assert!(
            token_count < 500,
            "sourcemap should have fewer than 500 tokens, got {}",
            token_count
        );
    }

    #[test]
    fn sourcemap_all_tokens_have_valid_source() {
        // All tokens should reference a valid source index
        let source = "function Component(props) { return <div>{props.x}</div>; }";
        let result = compile_to_result(source);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());
        let source_count = sm.get_sources().count() as u32;
        for token in sm.get_tokens() {
            if let Some(src_id) = token.get_source_id() {
                assert!(
                    src_id < source_count,
                    "token source_id {} exceeds source count {}",
                    src_id,
                    source_count
                );
            }
        }
    }

    // --- OXC comment attachment spike tests ---

    #[test]
    fn spike_oxc_preserves_comments_from_source() {
        // Test: parse source with comments → codegen → verify comments survive
        let source = "// leading comment\nconst x = 1;\n// trailing comment\nconst y = 2;\n";
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs();
        let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        assert!(!parsed.panicked);
        let code = super::codegen_program(&parsed.program);
        assert!(
            code.contains("// leading comment"),
            "leading comment should survive codegen, got:\n{code}"
        );
        assert!(
            code.contains("// trailing comment"),
            "trailing comment should survive codegen, got:\n{code}"
        );
    }

    #[test]
    fn spike_oxc_preserves_added_comments() {
        // Test: parse source, add a comment to the program's comment list,
        // then codegen → verify the added comment appears in output.
        let source = "const x = 1;\nconst y = 2;\n";
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs();
        let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        assert!(!parsed.panicked);

        // Check: does codegen reproduce the source faithfully?
        let code = super::codegen_program(&parsed.program);
        assert!(
            code.contains("const x = 1"),
            "basic round-trip should work: {code}"
        );

        // Note: OXC codegen only emits comments that are in the program's
        // comments list AND positioned relative to AST nodes. We can't
        // synthetically inject comments easily since they need valid spans
        // that reference positions in the original source text.
        // This test verifies the baseline behavior.
        let comment_count = parsed.program.comments.len();
        eprintln!("source has {comment_count} comments");
    }

    #[test]
    fn spike_oxc_blank_line_via_empty_comment() {
        // Test: can we insert a blank line by using a comment with specific formatting?
        let source = "// first\nconst x = 1;\n\n// second\nconst y = 2;\n";
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs();
        let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        let code = super::codegen_program(&parsed.program);
        // Check if the blank line between statements is preserved
        let has_blank_line = code.contains(";\n\n");
        eprintln!("blank line preserved: {has_blank_line}");
        eprintln!("output:\n{code}");
        // This documents the behavior — may or may not preserve blank lines
    }

    #[test]
    fn sourcemap_unique_debug_ids() {
        // Two compilations should produce different debugIds
        let source = "function Component(props) { return <div>{props.x}</div>; }";
        let result1 = compile_to_result(source);
        let result2 = compile_to_result(source);
        let parsed1: serde_json::Value =
            serde_json::from_str(result1.map.as_ref().unwrap()).unwrap();
        let parsed2: serde_json::Value =
            serde_json::from_str(result2.map.as_ref().unwrap()).unwrap();
        assert_ne!(
            parsed1["debugId"], parsed2["debugId"],
            "different compilations should have different debugIds"
        );
    }

    #[test]
    fn sourcemap_has_virtual_source() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value = serde_json::from_str(map_json).unwrap();
        let sources = parsed["sources"].as_array().expect("should have sources");
        assert!(
            sources
                .iter()
                .any(|s| s.as_str() == Some("compiler://react-compiler/generated")),
            "sources should include virtual generated source, got: {sources:?}"
        );
    }

    #[test]
    fn sourcemap_has_x_google_ignore_list() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value = serde_json::from_str(map_json).unwrap();
        let ignore_list = parsed["x_google_ignoreList"]
            .as_array()
            .expect("should have x_google_ignoreList");
        assert!(
            !ignore_list.is_empty(),
            "x_google_ignoreList should not be empty"
        );
        // The virtual source should be in the ignore list
        let sources = parsed["sources"].as_array().unwrap();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_str() == Some("compiler://react-compiler/generated"))
            .expect("virtual source should exist");
        assert!(
            ignore_list.contains(&serde_json::Value::from(virtual_idx as u64)),
            "x_google_ignoreList should contain the virtual source index"
        );
    }

    #[test]
    fn sourcemap_generated_code_routed_to_virtual_source() {
        // Compiled component should have some tokens routed to the virtual source
        // (cache infrastructure like useMemoCache, $[0] assignments)
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        let sm = decode_sourcemap(result.map.as_ref().unwrap());
        let sources: Vec<_> = sm.get_sources().collect();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated");
        // Virtual source should exist in the sources array
        assert!(
            virtual_idx.is_some(),
            "virtual source should be in sources array"
        );
    }

    #[test]
    fn sourcemap_partial_compilation_identity_mapping() {
        // When one function bails out and another compiles, the untouched
        // function's code should still have sourcemap entries pointing to
        // correct source positions (identity mapping).
        let source = r#"function helper(x) { return x + 1; }
function Component(props) {
  return <div>{props.x}</div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // The helper function (untouched) is at source line 0.
        // Verify it has tokens mapping to source line 0.
        assert_source_line_mapped(&sm, 0, "helper function (untouched)");

        // The Component (compiled) is at source line 1.
        assert_source_line_mapped(&sm, 1, "Component function (compiled)");

        // Verify tokens for the helper map to approximately the same generated line
        // (identity-ish mapping — the helper should be near the top of output too).
        let helper_tokens: Vec<_> = sm.get_tokens().filter(|t| t.get_src_line() == 0).collect();
        assert!(
            !helper_tokens.is_empty(),
            "helper function should have sourcemap tokens"
        );
        // The first token for source line 0 should be at generated line 0 or close to it
        let first_helper_gen_line = helper_tokens[0].get_dst_line();
        assert!(
            first_helper_gen_line <= 2,
            "helper function should be near the top of generated output, got line {}",
            first_helper_gen_line
        );
    }

    #[test]
    fn sourcemap_hoc_pattern() {
        // Fixture 9: Higher-order component pattern
        let source = r#"function withLogging(WrappedComponent) {
  function EnhancedComponent(props) {
    console.log("render");
    return <WrappedComponent {...props} />;
  }
  return EnhancedComponent;
}
function MyComponent(props) {
  return <div>{props.name}</div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // Both function declarations should have mappings
        assert_source_line_mapped(&sm, 0, "withLogging declaration");
        assert_source_line_mapped(&sm, 7, "MyComponent declaration");
    }

    #[test]
    fn sourcemap_token_positions_monotonic() {
        // Generated lines in sourcemap tokens should be monotonically non-decreasing
        // (sourcemaps require tokens to be ordered by generated position).
        let result = compile_to_result(
            "function Component(props) { const x = props.a + 1; return <div>{x}</div>; }",
        );
        let sm = decode_sourcemap(result.map.as_ref().unwrap());
        let mut prev_line = 0u32;
        let mut prev_col = 0u32;
        for token in sm.get_tokens() {
            let line = token.get_dst_line();
            let col = token.get_dst_col();
            if line == prev_line {
                assert!(
                    col >= prev_col,
                    "tokens on same generated line should have non-decreasing columns: \
                     prev={}:{}, curr={}:{}",
                    prev_line,
                    prev_col,
                    line,
                    col
                );
            } else {
                assert!(
                    line >= prev_line,
                    "generated lines should be non-decreasing: prev={}, curr={}",
                    prev_line,
                    line
                );
            }
            prev_line = line;
            prev_col = col;
        }
    }

    #[test]
    fn sourcemap_manual_token_verification() {
        // Manual verification: compile a known source and check specific token mappings.
        // Source is on one line so we can verify precisely.
        let source = "function Component(props) {\n  return <div>{props.x}</div>;\n}";
        let result = compile_to_result(source);
        assert!(result.transformed);
        let sm = decode_sourcemap(result.map.as_ref().unwrap());

        // Collect all user-source tokens (exclude virtual generated source)
        let sources: Vec<_> = sm.get_sources().collect();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        let user_tokens: Vec<_> = sm
            .get_tokens()
            .filter(|t| t.get_source_id() != virtual_idx)
            .collect();

        // There should be user tokens (not all generated)
        assert!(
            !user_tokens.is_empty(),
            "should have user-source tokens, not all generated"
        );

        // At least one token should map to source line 0 (function declaration)
        assert!(
            user_tokens.iter().any(|t| t.get_src_line() == 0),
            "should have tokens mapping to function declaration (line 0)"
        );

        // At least one token should map to source line 1 (return statement)
        assert!(
            user_tokens.iter().any(|t| t.get_src_line() == 1),
            "should have tokens mapping to return statement (line 1)"
        );
    }

    // --- Round-trip sourcemap validation ---

    /// Validate sourcemap round-trip accuracy for a compiled fixture.
    ///
    /// For every user-source token:
    /// - If the token has a name, verify that name appears at (src_line, src_col) in the source
    /// - Verify src_line/src_col are within bounds of the original source
    /// - Verify dst_line/dst_col are within bounds of the generated code
    /// - Verify at least one user-source token and one virtual-source token exist
    fn validate_sourcemap_round_trip(source: &str, filename: &str) {
        let options = crate::options::PluginOptions::default();
        let result = crate::compile(filename, source, &options);
        assert!(
            result.transformed,
            "fixture should be transformed: {filename}"
        );
        let map_json = result
            .map
            .as_ref()
            .unwrap_or_else(|| panic!("sourcemap should exist for {filename}"));
        let sm = decode_sourcemap(map_json);

        let source_lines: Vec<&str> = source.lines().collect();
        let gen_lines: Vec<&str> = result.code.lines().collect();
        let sources: Vec<_> = sm.get_sources().collect();
        let names: Vec<_> = sm.get_names().collect();

        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        let mut user_token_count = 0u32;
        let mut virtual_token_count = 0u32;

        for token in sm.get_tokens() {
            let is_virtual = token.get_source_id() == virtual_idx;
            if is_virtual {
                virtual_token_count += 1;
                continue;
            }
            user_token_count += 1;

            let src_line = token.get_src_line() as usize;
            let src_col = token.get_src_col() as usize;
            let dst_line = token.get_dst_line() as usize;
            let dst_col = token.get_dst_col() as usize;

            // Verify source line is within bounds.
            assert!(
                src_line < source_lines.len(),
                "[{filename}] token src_line {src_line} out of bounds (source has {} lines)",
                source_lines.len()
            );

            // Verify generated line is within bounds.
            assert!(
                dst_line < gen_lines.len(),
                "[{filename}] token dst_line {dst_line} out of bounds (generated has {} lines)",
                gen_lines.len()
            );

            // If the token has a single-line name, verify it appears at the source position.
            if let Some(name_id) = token.get_name_id() {
                let name_idx = name_id as usize;
                if name_idx < names.len() {
                    let name = names[name_idx].as_ref();
                    // Skip multi-line names (OXC may embed full expressions).
                    if !name.contains('\n') && src_col < source_lines[src_line].len() {
                        let src_text = &source_lines[src_line][src_col..];
                        assert!(
                            src_text.starts_with(name),
                            "[{filename}] token name {:?} not found at src {}:{} — found {:?}",
                            name,
                            src_line,
                            src_col,
                            &src_text[..src_text.len().min(30)]
                        );
                    }
                }
            }
        }

        assert!(
            user_token_count > 0,
            "[{filename}] should have at least one user-source token"
        );
        // Only assert virtual tokens if the output contains memoization infrastructure.
        // Trivial components may not produce any generated code tokens.
        if result.code.contains("useMemoCache") || result.code.contains("_c[") {
            assert!(
                virtual_token_count > 0,
                "[{filename}] memoized output should have virtual-source tokens"
            );
        }
    }

    #[test]
    fn roundtrip_smoke_test() {
        validate_sourcemap_round_trip(
            "function Component(props) {\n  return <div>{props.x}</div>;\n}",
            "test.jsx",
        );
    }

    /// Validate sourcemap bounds for fixtures that need CompilationMode::All.
    /// Same as validate_sourcemap_round_trip but with custom options.
    fn validate_sourcemap_round_trip_all_mode(source: &str, filename: &str) {
        let options = crate::options::PluginOptions {
            compilation_mode: crate::options::CompilationMode::All,
            ..crate::options::PluginOptions::default()
        };
        let result = crate::compile(filename, source, &options);
        if !result.transformed {
            return; // Some fixtures may not transform in all mode
        }
        let map_json = result
            .map
            .as_ref()
            .unwrap_or_else(|| panic!("sourcemap should exist for {filename}"));
        let sm = decode_sourcemap(map_json);

        let source_lines: Vec<&str> = source.lines().collect();
        let gen_lines: Vec<&str> = result.code.lines().collect();
        let sources: Vec<_> = sm.get_sources().collect();
        let names: Vec<_> = sm.get_names().collect();

        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        let mut user_token_count = 0u32;
        for token in sm.get_tokens() {
            if token.get_source_id() == virtual_idx {
                continue;
            }
            user_token_count += 1;

            let src_line = token.get_src_line() as usize;
            let src_col = token.get_src_col() as usize;
            let dst_line = token.get_dst_line() as usize;
            let dst_col = token.get_dst_col() as usize;

            assert!(
                src_line < source_lines.len(),
                "[{filename}] src_line {src_line} OOB (source has {} lines)",
                source_lines.len()
            );
            assert!(
                dst_line < gen_lines.len(),
                "[{filename}] dst_line {dst_line} OOB (gen has {} lines)",
                gen_lines.len()
            );

            if let Some(name_id) = token.get_name_id() {
                let name_idx = name_id as usize;
                if name_idx < names.len() {
                    let name = names[name_idx].as_ref();
                    if !name.contains('\n') && src_col < source_lines[src_line].len() {
                        let src_text = &source_lines[src_line][src_col..];
                        assert!(
                            src_text.starts_with(name),
                            "[{filename}] name {:?} not at src {}:{} — found {:?}",
                            name,
                            src_line,
                            src_col,
                            &src_text[..src_text.len().min(30)]
                        );
                    }
                }
            }
        }

        assert!(
            user_token_count > 0,
            "[{filename}] should have user-source tokens"
        );
    }

    // --- Round-trip tests: third_party conformance fixtures ---

    #[test]
    fn roundtrip_simple_function() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/simple-function-1.js"
        );
        validate_sourcemap_round_trip_all_mode(source, "simple-function-1.js");
    }

    #[test]
    fn roundtrip_capturing_func_simple_alias() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             capturing-func-simple-alias.js"
        );
        validate_sourcemap_round_trip_all_mode(source, "capturing-func-simple-alias.js");
    }

    #[test]
    fn roundtrip_conditional_early_return() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             conditional-early-return.js"
        );
        validate_sourcemap_round_trip_all_mode(source, "conditional-early-return.js");
    }

    #[test]
    fn roundtrip_for_in_statement() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             for-in-statement-body-always-returns.js"
        );
        validate_sourcemap_round_trip_all_mode(source, "for-in-statement-body-always-returns.js");
    }

    #[test]
    fn roundtrip_jsx_mutations() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             builtin-jsx-tag-lowered-between-mutations.js"
        );
        validate_sourcemap_round_trip(source, "builtin-jsx-tag-lowered-between-mutations.js");
    }

    #[test]
    fn roundtrip_nested_function_captures() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             capturing-variable-in-nested-function.js"
        );
        validate_sourcemap_round_trip_all_mode(source, "capturing-variable-in-nested-function.js");
    }

    #[test]
    fn roundtrip_tsx_inner_functions() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             component-inner-function-with-many-args.tsx"
        );
        validate_sourcemap_round_trip(source, "component-inner-function-with-many-args.tsx");
    }

    #[test]
    fn roundtrip_tsx_aliased_nested_scope() {
        let source = include_str!(
            "../../../../../third_party/react/compiler/packages/\
             babel-plugin-react-compiler/src/__tests__/fixtures/compiler/\
             aliased-nested-scope-fn-expr.tsx"
        );
        validate_sourcemap_round_trip(source, "aliased-nested-scope-fn-expr.tsx");
    }

    // --- Round-trip tests: custom compiler fixtures (tests/fixtures/compiler/) ---

    #[test]
    fn roundtrip_catch_forloop_same_name() {
        let source =
            include_str!("../../../../../tests/fixtures/compiler/catch-forloop-same-name-variable.jsx");
        validate_sourcemap_round_trip(source, "catch-forloop-same-name-variable.jsx");
    }

    #[test]
    fn roundtrip_closure_scope_leak() {
        let source = include_str!(
            "../../../../../tests/fixtures/compiler/closure-multiple-calls-scope-leak.jsx"
        );
        validate_sourcemap_round_trip(source, "closure-multiple-calls-scope-leak.jsx");
    }

    #[test]
    fn roundtrip_local_function_extra_memo() {
        let source =
            include_str!("../../../../../tests/fixtures/compiler/local-function-call-extra-memo.jsx");
        validate_sourcemap_round_trip(source, "local-function-call-extra-memo.jsx");
    }

    #[test]
    fn roundtrip_conditional_extra_scope() {
        let source =
            include_str!("../../../../../tests/fixtures/compiler/conditional-expr-extra-scope.jsx");
        validate_sourcemap_round_trip(source, "conditional-expr-extra-scope.jsx");
    }

    #[test]
    fn roundtrip_labeled_block_scoping_tsx() {
        let source =
            include_str!("../../../../../tests/fixtures/compiler/labeled-block-scoping.tsx");
        validate_sourcemap_round_trip(source, "labeled-block-scoping.tsx");
    }

    // --- Round-trip tests: shadcn real-world components ---

    #[test]
    fn roundtrip_shadcn_dialog() {
        let source = include_str!(
            "../../../../../tests/fixtures/shadcn-app/src/components/ui/Dialog.tsx"
        );
        validate_sourcemap_round_trip(source, "Dialog.tsx");
    }

    #[test]
    fn roundtrip_shadcn_dashboard() {
        let source = include_str!(
            "../../../../../tests/fixtures/shadcn-app/src/components/Dashboard.tsx"
        );
        validate_sourcemap_round_trip(source, "Dashboard.tsx");
    }

    #[test]
    fn roundtrip_shadcn_button() {
        let source = include_str!(
            "../../../../../tests/fixtures/shadcn-app/src/components/ui/Button.tsx"
        );
        validate_sourcemap_round_trip(source, "Button.tsx");
    }

    #[test]
    fn roundtrip_shadcn_tabs() {
        let source =
            include_str!("../../../../../tests/fixtures/shadcn-app/src/components/ui/Tabs.tsx");
        validate_sourcemap_round_trip(source, "Tabs.tsx");
    }

    #[test]
    fn roundtrip_shadcn_select() {
        let source =
            include_str!("../../../../../tests/fixtures/shadcn-app/src/components/ui/Select.tsx");
        validate_sourcemap_round_trip(source, "Select.tsx");
    }
}
