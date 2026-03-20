use std::collections::HashSet;

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, ast};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};

use super::postprocess::codegen_program;
use crate::codegen_backend::CompiledFunction;

pub(super) fn move_leading_comment_to_import_trailing(code: &str) -> String {
    if !code.starts_with("import ") {
        return code.to_string();
    }

    // Find the last import line in the consecutive import block that ends with a
    // compiler-runtime import. Only apply comment-move when we inserted a runtime import.
    let lines: Vec<&str> = code.lines().collect();
    let mut last_runtime_import_idx: Option<usize> = None;
    let mut last_import_idx = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("import ") {
            last_import_idx = i;
            if line.contains("react/compiler-runtime") || line.contains("react-compiler-runtime") {
                last_runtime_import_idx = Some(i);
            }
        } else {
            break;
        }
    }

    // Only apply if we have a runtime import, and the comment follows the last import
    // in the block. Use last_import_idx (not last_runtime_import_idx) since Babel
    // attaches the comment to whichever import ends up last in the output.
    if last_runtime_import_idx.is_none() {
        return code.to_string();
    }

    // Check if the line after the last import is a comment (line or block).
    // Skip blank lines between the last import and the comment.
    let mut comment_idx = last_import_idx + 1;
    while comment_idx < lines.len() && lines[comment_idx].trim().is_empty() {
        comment_idx += 1;
    }
    if comment_idx >= lines.len() {
        return code.to_string();
    }
    let is_line_comment = lines[comment_idx].starts_with("//");
    let is_block_comment =
        lines[comment_idx].starts_with("/**") || lines[comment_idx].starts_with("/*");
    if !is_line_comment && !is_block_comment {
        return code.to_string();
    }

    // For block comments, find the end line (the line containing */)
    let comment_end_idx = if is_block_comment {
        let mut end = comment_idx;
        for (j, line) in lines.iter().enumerate().skip(comment_idx) {
            if line.contains("*/") {
                end = j;
                break;
            }
        }
        end
    } else {
        comment_idx
    };

    // Build the result: everything up to last import, then import + first comment line,
    // then remaining comment lines, then rest. Skip blank lines between the import
    // and the comment that were jumped over during detection.
    let mut result = String::with_capacity(code.len());
    for (i, line) in lines.iter().enumerate() {
        if i == last_import_idx {
            result.push_str(line);
            result.push(' ');
            result.push_str(lines[comment_idx]);
            result.push('\n');
        } else if i == comment_idx {
            // Skip — already merged onto import line
            continue;
        } else if i > last_import_idx && i < comment_idx && lines[i].trim().is_empty() {
            // Skip blank lines between last import and the merged comment
            continue;
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    // Remove trailing newline if original didn't have one
    if !code.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    let _ = comment_end_idx; // block comment lines after first are kept in place
    result
}

/// Insert blank lines in the output based on blank line positions in the original source.
///
/// When the original source has a blank line between two statements, and both of those
/// statements appear (by text match) in the compiled output, we insert a blank line in
/// the output between them. This mirrors Babel's behavior of preserving source-location-
/// based blank lines.
pub(super) fn transfer_blank_lines_from_original_source(
    code: &str,
    source: &str,
    compiled: &[CompiledFunction],
) -> String {
    if compiled.is_empty() {
        return code.to_string();
    }

    // Collect pairs of consecutive code lines that have a line gap > 1 in the
    // original source. This handles two cases:
    //   1. Blank lines between code statements
    //   2. Comments between code statements (which get removed during compilation,
    //      but Babel preserves the line gap they create)
    let mut blank_line_pairs: HashSet<(String, String)> = HashSet::new();

    let is_code_line = |s: &str| -> bool {
        let t = s.trim();
        !t.is_empty()
            && !t.starts_with("//")
            && !t.starts_with("/*")
            && !t.starts_with("* ")
            && !t.starts_with("*/")
    };

    for cf in compiled {
        let start = cf.start as usize;
        let end = cf.end as usize;
        if start >= source.len() || end > source.len() || start >= end {
            continue;
        }
        let func_source = &source[start..end];
        let lines: Vec<&str> = func_source.lines().collect();

        // Find all code lines (non-blank, non-comment) and their line numbers
        let code_lines_with_idx: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| is_code_line(l))
            .map(|(i, l)| (i, *l))
            .collect();

        // For each pair of consecutive code lines with a gap > 1, record
        // them as a blank-line pair
        for window in code_lines_with_idx.windows(2) {
            let (line_a, text_a) = window[0];
            let (line_b, text_b) = window[1];
            if line_b > line_a + 1 {
                blank_line_pairs.insert((text_a.trim().to_string(), text_b.trim().to_string()));
            }
        }
    }

    if std::env::var("DEBUG_TRANSFER_BLANK_SRC").is_ok() {
        eprintln!("=== transfer_blank_lines_from_original_source ===");
        eprintln!("blank_line_pairs ({}):", blank_line_pairs.len());
        for (before, after) in &blank_line_pairs {
            eprintln!("  ({:?}, {:?})", before, after);
        }
    }

    if blank_line_pairs.is_empty() {
        return code.to_string();
    }

    // Normalize a trimmed line for pair matching:
    // - Strip let/const/var prefixes (compiler may change declaration kinds)
    // - Strip `return ` prefix (return values become temp assignments)
    // - Strip `tN = ` prefix (temp assignment targets, where N is digits)
    // - Trim trailing semicolons (codegen may omit them)
    let normalize_for_match = |s: &str| -> String {
        let s = s
            .strip_prefix("let ")
            .or_else(|| s.strip_prefix("const "))
            .or_else(|| s.strip_prefix("var "))
            .unwrap_or(s);
        let s = s.strip_prefix("return ").unwrap_or(s);
        // Strip `tN = ` where N is one or more digits
        let s = if let Some(rest) = s.strip_prefix('t') {
            if let Some(eq_pos) = rest.find(" = ") {
                if !rest[..eq_pos].is_empty() && rest[..eq_pos].chars().all(|c| c.is_ascii_digit())
                {
                    &rest[eq_pos + 3..]
                } else {
                    s
                }
            } else {
                s
            }
        } else {
            s
        };
        let s = s.strip_suffix(';').unwrap_or(s);
        s.to_string()
    };

    // Re-index pairs with normalized keys.
    // Each original pair can produce multiple normalizations because the
    // `return expr` → `tN = expr` transform means either side might be
    // a return or a temp assignment.  We store all normalizations so a
    // match on the compiled output side works regardless of which form
    // appears.
    let normalized_pairs: HashSet<(String, String)> = blank_line_pairs
        .iter()
        .map(|(before, after)| (normalize_for_match(before), normalize_for_match(after)))
        .collect();

    // Track whether we're inside a scope computation body (if/else block that
    // tests cache slots). We only want to insert blank lines inside these blocks,
    // not at the top level of the function body, to avoid false positives.
    let is_scope_test_line = |line: &str| -> bool {
        let t = line.trim();
        // Match patterns like: if ($[0] === Symbol... or if ($[0] !== ...
        (t.starts_with("if ($[") || t.starts_with("if (($["))
            && (t.contains("Symbol.for") || t.contains("!=="))
    };

    let code_lines: Vec<&str> = code.lines().collect();
    let mut result = String::with_capacity(code.len() + blank_line_pairs.len() * 2);
    let mut scope_body_depth: i32 = 0;
    let mut in_scope_body = false;
    let mut i = 0;
    while i < code_lines.len() {
        let current_trimmed = code_lines[i].trim();

        // Track scope body state
        if is_scope_test_line(code_lines[i]) {
            in_scope_body = true;
            scope_body_depth = 0;
        }
        if in_scope_body {
            for ch in current_trimmed.chars() {
                match ch {
                    '{' => scope_body_depth += 1,
                    '}' => {
                        scope_body_depth -= 1;
                        if scope_body_depth <= 0 {
                            in_scope_body = false;
                        }
                    }
                    _ => {}
                }
            }
        }

        result.push_str(code_lines[i]);
        result.push('\n');

        // Only insert blank lines when we're inside a scope computation body
        if in_scope_body && scope_body_depth > 0 && !current_trimmed.is_empty() {
            // Find next non-blank line
            let next_non_blank =
                ((i + 1)..code_lines.len()).find(|&j| !code_lines[j].trim().is_empty());
            if let Some(next_idx) = next_non_blank {
                let next_trimmed = code_lines[next_idx].trim();
                // Check if there should be a blank line here but isn't
                let has_blank_between = (i + 1..next_idx).any(|j| code_lines[j].trim().is_empty());
                if !has_blank_between
                    && normalized_pairs.contains(&(
                        normalize_for_match(current_trimmed),
                        normalize_for_match(next_trimmed),
                    ))
                {
                    result.push('\n');
                }
            }
        }
        i += 1;
    }

    // Handle trailing newline
    if !code.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }

    result
}

pub(super) fn apply_internal_blank_line_markers(code: &str) -> String {
    let marker_line = format!(
        "\"{}\";",
        crate::codegen_backend::codegen_ast::INTERNAL_BLANK_LINE_MARKER
    );
    if !code.lines().any(|line| line.trim() == marker_line) {
        return code.to_string();
    }

    let mut result = String::with_capacity(code.len());
    for line in code.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim() == marker_line {
            result.push('\n');
        } else {
            result.push_str(line);
        }
    }

    if !code.ends_with('\n') && result.ends_with('\n') {
        let last_line_is_marker = code
            .lines()
            .last()
            .is_some_and(|line| line.trim() == marker_line);
        if !last_line_is_marker {
            result.pop();
        }
    }

    result
}

/// Apply blank line markers to the generated code.
///
/// `blank_line_before[i]` indicates that the i-th top-level statement in the output
/// body should be preceded by a blank line. This re-parses the generated code to find
/// statement boundaries and inserts `\n` at the right positions.
pub(super) fn apply_blank_line_markers(
    source_type: SourceType,
    code: &str,
    blank_line_before: &[bool],
) -> String {
    if !blank_line_before.iter().any(|&b| b) {
        return code.to_string();
    }

    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type).parse();
    // Only bail on panic; recoverable parse errors (e.g. labeled const
    // declarations emitted by the compiler) still produce a usable AST with
    // correct top-level statement spans.
    if parsed.panicked {
        return code.to_string();
    }

    let stmts = &parsed.program.body;
    let top_level_comments = parsed
        .program
        .comments
        .iter()
        .filter(|comment| {
            !stmts.iter().any(|stmt| {
                let span = stmt.span();
                comment.span.start >= span.start && comment.span.end <= span.end
            })
        })
        .collect::<Vec<_>>();
    let mut insert_positions = Vec::new();
    for (i, stmt) in stmts.iter().enumerate() {
        if i >= blank_line_before.len() || !blank_line_before[i] {
            continue;
        }
        if i == 0 {
            let gap_start = top_level_comments
                .iter()
                .filter(|comment| comment.span.end <= stmt.span().start)
                .map(|comment| comment.span.end as usize)
                .max();
            let Some(gap_start) = gap_start else {
                continue;
            };
            let curr_start = stmt.span().start as usize;
            let between = &code[gap_start..curr_start];
            let newline_count = between.chars().filter(|&c| c == '\n').count();
            if newline_count < 2 {
                if let Some(nl_pos) = between.find('\n') {
                    insert_positions.push(gap_start + nl_pos + 1);
                } else {
                    insert_positions.push(gap_start);
                }
            }
        } else {
            let prev_end = stmts[i - 1].span().end as usize;
            let curr_start = stmt.span().start as usize;

            // When there are comments between the previous statement and the
            // current one, the blank line from the original source is
            // typically between the previous statement and the FIRST comment
            // — not between the last comment and the current statement. Check
            // the gap from prev_end first.
            let comments_between: Vec<_> = top_level_comments
                .iter()
                .filter(|comment| {
                    comment.span.start >= prev_end as u32 && comment.span.end <= stmt.span().start
                })
                .collect();

            if !comments_between.is_empty() {
                // Check for blank between prev_end and first comment
                let first_comment_start = comments_between
                    .iter()
                    .map(|c| c.span.start as usize)
                    .min()
                    .unwrap();
                let before_first_comment = &code[prev_end..first_comment_start];
                let nl_before = before_first_comment.chars().filter(|&c| c == '\n').count();
                if nl_before < 2
                    && let Some(nl_pos) = before_first_comment.find('\n')
                {
                    insert_positions.push(prev_end + nl_pos + 1);
                }
            } else {
                // No comments between — simple case
                let between = &code[prev_end..curr_start];
                let newline_count = between.chars().filter(|&c| c == '\n').count();
                if newline_count < 2
                    && let Some(nl_pos) = between.find('\n')
                {
                    insert_positions.push(prev_end + nl_pos + 1);
                }
            }
        }
    }

    if insert_positions.is_empty() {
        return code.to_string();
    }

    let mut result = String::with_capacity(code.len() + insert_positions.len());
    let mut cursor = 0;
    for pos in insert_positions {
        result.push_str(&code[cursor..pos]);
        result.push('\n');
        cursor = pos;
    }
    result.push_str(&code[cursor..]);
    result
}

pub(super) fn codegen_statement_source(
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
