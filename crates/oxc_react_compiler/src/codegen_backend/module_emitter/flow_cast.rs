use oxc_allocator::Allocator;
use oxc_ast::ast;
use oxc_span::SourceType;

use super::postprocess::parse_statements;
use super::transform_flag::strip_trailing_void_return_from_initializer_expression;

pub(super) const FLOW_CAST_MARKER_HELPER: &str = "__REACT_COMPILER_FLOW_CAST__";
pub(super) const FLOW_CAST_REWRITE_MARKER_COMMENT: &str = "/*__FLOW_CAST__*/";

pub(super) fn parse_expression_source<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    expr_source: &str,
) -> Result<ast::Expression<'a>, String> {
    let ts_source_type = source_type.with_typescript(true);
    let mut attempts = vec![
        (source_type, expr_source.to_string()),
        (ts_source_type, expr_source.to_string()),
    ];
    let flow_cast_normalized = normalize_generated_body_flow_cast_marker_calls(expr_source);
    if flow_cast_normalized != expr_source {
        attempts.push((ts_source_type, flow_cast_normalized.clone()));
    }
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(expr_source);
    if flow_cast_rewritten != expr_source && flow_cast_rewritten != flow_cast_normalized {
        attempts.push((ts_source_type, flow_cast_rewritten));
    }
    for (attempt_source_type, attempt_expr) in attempts {
        let wrapped = format!("({attempt_expr});");
        let Ok(mut statements) = parse_statements(
            allocator,
            attempt_source_type,
            allocator.alloc_str(&wrapped),
        ) else {
            continue;
        };
        let Some(ast::Statement::ExpressionStatement(statement)) = statements.pop() else {
            continue;
        };
        let mut expression = statement.unbox().expression;
        loop {
            match expression {
                ast::Expression::ParenthesizedExpression(parenthesized)
                    if matches!(
                        parenthesized.expression.without_parentheses(),
                        ast::Expression::ArrowFunctionExpression(_)
                    ) =>
                {
                    expression = parenthesized.unbox().expression;
                }
                _ => break,
            }
        }
        strip_trailing_void_return_from_initializer_expression(&mut expression);
        return Ok(expression);
    }
    Err("failed to parse expression snippet".to_string())
}

#[cfg(test)]
pub(super) fn normalize_generated_body_iife_parenthesization(body_source: &str) -> String {
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

pub(super) fn normalize_generated_body_flow_cast_marker_calls(body_source: &str) -> String {
    let rewritten = crate::pipeline::rewrite_flow_cast_expressions(body_source);
    if rewritten == body_source {
        return body_source.to_string();
    }
    rewrite_flow_cast_marker_calls(&rewritten)
}

pub(super) fn rewrite_flow_cast_marker_calls(source: &str) -> String {
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

pub(super) fn restore_flow_cast_marker_calls(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if source[i..].starts_with(FLOW_CAST_REWRITE_MARKER_COMMENT) {
            while out.chars().last().is_some_and(char::is_whitespace) {
                out.pop();
            }
            i += FLOW_CAST_REWRITE_MARKER_COMMENT.len();
            i = skip_ascii_whitespace(source, i);
            continue;
        }

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
