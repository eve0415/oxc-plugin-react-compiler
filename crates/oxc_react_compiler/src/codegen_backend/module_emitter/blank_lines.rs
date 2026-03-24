use std::collections::HashSet;

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, ast};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SPAN, SourceType};

use super::postprocess::codegen_program;
use crate::codegen_backend::CompiledFunction;

enum LeadingFileCommentStyle {
    None,
    IsolatedLine,
    CommentGroupWithBlankGap,
    Block,
    ImportTrailingLine,
}

fn first_import_trailing_line_comment(source: &str) -> Option<String> {
    let first_line = source.lines().find(|line| !line.trim().is_empty())?;
    let trimmed = first_line.trim_start();
    if !trimmed.starts_with("import ") {
        return None;
    }
    let comment_idx = trimmed.find("//")?;
    Some(trimmed[comment_idx..].to_string())
}

fn leading_file_comment_style(source: &str) -> LeadingFileCommentStyle {
    let trimmed = source.trim_start();
    let mut rest = trimmed;
    if rest.starts_with("/**") || rest.starts_with("/*") {
        return LeadingFileCommentStyle::Block;
    }
    if !rest.starts_with("//") {
        if let Some(first_line) = trimmed.lines().find(|line| !line.trim().is_empty()) {
            let first_trimmed = first_line.trim_start();
            if first_trimmed.starts_with("import ") && first_trimmed.contains("//") {
                return LeadingFileCommentStyle::ImportTrailingLine;
            }
        }
        return LeadingFileCommentStyle::None;
    }

    let mut comment_lines = 0usize;
    let mut saw_blank_after_comments = false;
    let mut next_noncomment_is_import = false;
    while !rest.is_empty() {
        let Some(line_end) = rest.find('\n') else {
            let trimmed = rest.trim_start();
            if trimmed.starts_with("//") {
                comment_lines += 1;
            } else if !trimmed.is_empty() {
                next_noncomment_is_import = trimmed.starts_with("import ");
            }
            break;
        };
        let line = &rest[..line_end];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            if comment_lines > 0 {
                saw_blank_after_comments = true;
            }
            rest = &rest[line_end + 1..];
            continue;
        }
        if trimmed.starts_with("//") {
            comment_lines += 1;
            rest = &rest[line_end + 1..];
            continue;
        }
        next_noncomment_is_import = trimmed.starts_with("import ");
        break;
    }

    if comment_lines == 1 {
        LeadingFileCommentStyle::IsolatedLine
    } else if comment_lines > 1 && (saw_blank_after_comments || !next_noncomment_is_import) {
        LeadingFileCommentStyle::CommentGroupWithBlankGap
    } else {
        LeadingFileCommentStyle::None
    }
}

pub(super) fn move_leading_comment_to_import_trailing(code: &str, source: &str) -> String {
    let leading_style = leading_file_comment_style(source);
    let import_trailing_comment = first_import_trailing_line_comment(source);
    if matches!(leading_style, LeadingFileCommentStyle::None) {
        return code.to_string();
    }
    if !code.starts_with("import ") {
        return code.to_string();
    }

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

    if last_runtime_import_idx.is_none() {
        return code.to_string();
    }

    let mut comment_idx = last_import_idx + 1;
    while comment_idx < lines.len() && lines[comment_idx].trim().is_empty() {
        comment_idx += 1;
    }
    let synthesized_import_comment = import_trailing_comment.as_deref().filter(|comment| {
        lines[last_import_idx].starts_with("import ") && !lines[last_import_idx].contains(*comment)
    });

    let merge_comment = |comment_text: &str, skip_comment_idx: Option<usize>| {
        let mut result = String::with_capacity(code.len() + comment_text.len() + 1);
        for (i, line) in lines.iter().enumerate() {
            if i == last_import_idx {
                result.push_str(line);
                result.push(' ');
                result.push_str(comment_text);
                result.push('\n');
            } else if Some(i) == skip_comment_idx
                || (skip_comment_idx.is_some()
                    && i > last_import_idx
                    && i < skip_comment_idx.unwrap()
                    && lines[i].trim().is_empty())
            {
                continue;
            } else {
                result.push_str(line);
                result.push('\n');
            }
        }
        if !code.ends_with('\n') && result.ends_with('\n') {
            result.pop();
        }
        result
    };

    if comment_idx >= lines.len() {
        return synthesized_import_comment
            .map(|comment| merge_comment(comment, None))
            .unwrap_or_else(|| code.to_string());
    }

    let comment_line = lines[comment_idx].trim_start();
    match leading_style {
        LeadingFileCommentStyle::IsolatedLine
        | LeadingFileCommentStyle::CommentGroupWithBlankGap
            if !comment_line.starts_with("//") =>
        {
            return code.to_string();
        }
        LeadingFileCommentStyle::ImportTrailingLine => {
            if comment_line.starts_with("//") {
                return merge_comment(lines[comment_idx], Some(comment_idx));
            }
            return synthesized_import_comment
                .map(|comment| merge_comment(comment, None))
                .unwrap_or_else(|| code.to_string());
        }
        LeadingFileCommentStyle::Block
            if !(comment_line.starts_with("/**") || comment_line.starts_with("/*")) =>
        {
            return code.to_string();
        }
        LeadingFileCommentStyle::None => return code.to_string(),
        LeadingFileCommentStyle::IsolatedLine
        | LeadingFileCommentStyle::CommentGroupWithBlankGap
        | LeadingFileCommentStyle::Block => {}
    }
    merge_comment(lines[comment_idx], Some(comment_idx))
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

/// Replace memoization comment marker statements with actual `//` comments.
///
/// Codegen emits `"__REACT_COMPILER_MEMO_COMMENT__:<text>";` as expression statements.
/// This function replaces each such line with `// <text>`, preserving indentation.
///
/// Special case: `"useMemo"` comments are appended as trailing inline comments
/// on the preceding `let` declaration line, matching Babel's output format:
///   `let x; // "useMemo" for t0 and x:`
pub(super) fn apply_memo_comment_markers(code: &str) -> String {
    let marker_prefix = crate::codegen_backend::codegen_ast::MEMO_COMMENT_MARKER;
    // Quick check — skip work if no markers present.
    if !code.contains(marker_prefix) {
        return code.to_string();
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut result = String::with_capacity(code.len());
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let comment_text = extract_memo_comment_text(trimmed, marker_prefix);
        if let Some(text) = comment_text {
            // "useMemo" comments: merge as trailing comment on previous `let` line.
            if text.starts_with("\"useMemo\"") || text.starts_with("\\\"useMemo\\\"") {
                // Unescape the text (OXC string codegen may escape inner quotes).
                let clean_text = text.replace("\\\"", "\"");
                // Try to merge with the preceding line if it's a `let` declaration.
                if result.ends_with('\n') {
                    // Find the last line in result.
                    let last_newline = result[..result.len() - 1].rfind('\n');
                    let last_line_start = last_newline.map_or(0, |p| p + 1);
                    let last_line = &result[last_line_start..result.len() - 1];
                    let last_trimmed = last_line.trim();
                    if last_trimmed.starts_with("let ") && last_trimmed.ends_with(';') {
                        // Remove the trailing newline, append comment, re-add newline.
                        result.pop(); // remove '\n'
                        result.push_str(" // ");
                        result.push_str(&clean_text);
                        result.push('\n');
                        i += 1;
                        continue;
                    }
                }
                // Fallback: emit as standalone comment line.
                let indent = &lines[i][..lines[i].len() - lines[i].trim_start().len()];
                result.push_str(indent);
                result.push_str("// ");
                result.push_str(&clean_text);
                result.push('\n');
            } else {
                // Regular comments: emit as standalone comment line.
                let clean_text = text.replace("\\\"", "\"");
                let indent = &lines[i][..lines[i].len() - lines[i].trim_start().len()];
                result.push_str(indent);
                result.push_str("// ");
                result.push_str(&clean_text);
                result.push('\n');
            }
        } else {
            result.push_str(lines[i]);
            result.push('\n');
        }
        i += 1;
    }
    // Remove trailing newline if original didn't have one.
    if !code.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Extract comment text from a memo comment marker line.
/// Returns `Some(text)` if the line matches `"__REACT_COMPILER_MEMO_COMMENT__:<text>";`.
fn extract_memo_comment_text<'a>(trimmed: &'a str, marker_prefix: &str) -> Option<&'a str> {
    // Pattern: `"marker:text";` or `"marker:text"` (without semicolon)
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix("\";").or_else(|| s.strip_suffix('"')))?;
    inner
        .strip_prefix(marker_prefix)
        .and_then(|s| s.strip_prefix(':'))
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
