use oxc_allocator::{Allocator, Dummy};
use oxc_ast::ast;
use oxc_ast_visit::{VisitMut, walk_mut};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

use super::flow_cast::rewrite_flow_cast_marker_calls;
use super::postprocess::codegen_program;

pub(super) fn compute_transform_state(
    source_type: SourceType,
    output_code: &str,
    source_untransformed: &str,
) -> bool {
    let output = normalize_module_for_transform_flag(source_type, output_code);
    let source = normalize_module_for_transform_flag(source_type, source_untransformed);
    if output.normalized == source.normalized {
        return false;
    }
    if let (Some(output_canonical), Some(source_canonical)) = (&output.canonical, &source.canonical)
        && output_canonical == source_canonical
    {
        return false;
    }
    match (output.canonical, source.canonical) {
        (Some(output_canonical), Some(source_canonical)) => output_canonical != source_canonical,
        _ => true,
    }
}

struct TransformFlagNormalization {
    normalized: String,
    canonical: Option<String>,
}

fn normalize_module_for_transform_flag(
    source_type: SourceType,
    code: &str,
) -> TransformFlagNormalization {
    let stripped = strip_nonsemantic_top_level_comments_for_transform_flag(source_type, code)
        .unwrap_or_else(|| StrippedTransformFlagCode {
            code: strip_leading_comments_for_transform_flag(code).to_string(),
            has_nested_comments: code.contains("//") || code.contains("/*"),
        });
    let flow_marker_rewritten = rewrite_flow_cast_marker_calls(
        &crate::pipeline::rewrite_flow_cast_expressions(&stripped.code),
    );
    let normalized = super::super::shared::normalize_for_transform_flag(&flow_marker_rewritten);
    let canonical = if stripped.has_nested_comments {
        None
    } else {
        canonicalize_module_for_transform_flag(source_type, &flow_marker_rewritten)
    };
    TransformFlagNormalization {
        normalized,
        canonical,
    }
}

struct StrippedTransformFlagCode {
    code: String,
    has_nested_comments: bool,
}

fn strip_nonsemantic_top_level_comments_for_transform_flag(
    source_type: SourceType,
    code: &str,
) -> Option<StrippedTransformFlagCode> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return None;
    }

    let statements = parsed
        .program
        .body
        .iter()
        .map(GetSpan::span)
        .collect::<Vec<_>>();
    let comments = parsed.program.comments.iter().collect::<Vec<_>>();
    let mut has_nested_comments = false;
    let mut stripped = String::with_capacity(code.len());
    let mut cursor = 0usize;

    for comment in comments {
        let comment_start = comment.span.start as usize;
        let comment_end = comment.span.end as usize;
        let is_nested = statements
            .iter()
            .any(|span| comment.span.start >= span.start && comment.span.end <= span.end);

        if is_nested {
            has_nested_comments = true;
            continue;
        }

        if cursor < comment_start {
            stripped.push_str(&code[cursor..comment_start]);
        }
        cursor = comment_end;
    }

    if cursor < code.len() {
        stripped.push_str(&code[cursor..]);
    }

    Some(StrippedTransformFlagCode {
        code: strip_leading_comments_for_transform_flag(&stripped).to_string(),
        has_nested_comments,
    })
}

fn canonicalize_module_for_transform_flag(source_type: SourceType, code: &str) -> Option<String> {
    try_canonicalize_module(source_type, code)
}

fn try_canonicalize_module(source_type: SourceType, code: &str) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return None;
    }
    let mut program = parsed.program;
    canonicalize_transform_flag_program(&allocator, &mut program);
    Some(super::super::shared::normalize_for_transform_flag(
        &codegen_program(&program),
    ))
}

fn canonicalize_transform_flag_program<'a>(
    allocator: &'a Allocator,
    program: &mut ast::Program<'a>,
) {
    let mut canonicalizer = TransformFlagCanonicalizer { allocator };
    canonicalizer.visit_program(program);
}

pub(super) fn canonicalize_initializer_expressions_in_statements<'a>(
    allocator: &'a Allocator,
    statements: &mut oxc_allocator::Vec<'a, ast::Statement<'a>>,
) {
    let mut canonicalizer = TransformFlagCanonicalizer { allocator };
    for statement in statements.iter_mut() {
        canonicalizer.visit_statement(statement);
    }
}

struct TransformFlagCanonicalizer<'a> {
    allocator: &'a Allocator,
}

impl<'a> VisitMut<'a> for TransformFlagCanonicalizer<'a> {
    fn visit_function(
        &mut self,
        function: &mut ast::Function<'a>,
        flags: oxc_syntax::scope::ScopeFlags,
    ) {
        walk_mut::walk_function(self, function, flags);
        if function.r#type == ast::FunctionType::FunctionExpression
            && let Some(body) = function.body.as_mut()
        {
            strip_trailing_void_return_from_function_body(body);
        }
    }

    fn visit_arrow_function_expression(&mut self, arrow: &mut ast::ArrowFunctionExpression<'a>) {
        walk_mut::walk_arrow_function_expression(self, arrow);
        strip_trailing_void_return_from_function_body(&mut arrow.body);
    }

    fn visit_variable_declarator(&mut self, declarator: &mut ast::VariableDeclarator<'a>) {
        walk_mut::walk_variable_declarator(self, declarator);
        if let Some(init) = declarator.init.as_mut() {
            canonicalize_initializer_expression(self.allocator, init);
        }
    }

    fn visit_assignment_expression(&mut self, expression: &mut ast::AssignmentExpression<'a>) {
        walk_mut::walk_assignment_expression(self, expression);
        canonicalize_initializer_expression(self.allocator, &mut expression.right);
    }
}

fn canonicalize_initializer_expression<'a>(
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
) {
    strip_wrapped_function_initializer_parens_ast(allocator, expression);
    strip_trailing_void_return_from_initializer_expression(expression);
}

fn strip_wrapped_function_initializer_parens_ast<'a>(
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
) -> bool {
    let mut stripped = false;
    while let ast::Expression::ParenthesizedExpression(parenthesized) = expression {
        if !matches!(
            parenthesized.expression.without_parentheses(),
            ast::Expression::ArrowFunctionExpression(_) | ast::Expression::FunctionExpression(_)
        ) {
            break;
        }
        let ast::Expression::ParenthesizedExpression(parenthesized) =
            std::mem::replace(expression, ast::Expression::dummy(allocator))
        else {
            unreachable!();
        };
        *expression = parenthesized.unbox().expression;
        stripped = true;
    }
    stripped
}

pub(super) fn strip_trailing_void_return_from_initializer_expression(
    expression: &mut ast::Expression<'_>,
) {
    match expression {
        ast::Expression::ParenthesizedExpression(parenthesized) => {
            strip_trailing_void_return_from_initializer_expression(&mut parenthesized.expression);
        }
        ast::Expression::ArrowFunctionExpression(arrow) => {
            strip_trailing_void_return_from_function_body(&mut arrow.body);
        }
        ast::Expression::FunctionExpression(function) => {
            if let Some(body) = function.body.as_mut() {
                strip_trailing_void_return_from_function_body(body);
            }
        }
        ast::Expression::TSAsExpression(ts_as) => {
            strip_trailing_void_return_from_initializer_expression(&mut ts_as.expression);
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies) => {
            strip_trailing_void_return_from_initializer_expression(&mut ts_satisfies.expression);
        }
        ast::Expression::TSNonNullExpression(ts_non_null) => {
            strip_trailing_void_return_from_initializer_expression(&mut ts_non_null.expression);
        }
        _ => {}
    }
}

fn strip_trailing_void_return_from_function_body(body: &mut ast::FunctionBody<'_>) -> bool {
    let Some(ast::Statement::ReturnStatement(return_statement)) = body.statements.last() else {
        return false;
    };
    if return_statement.argument.is_some() {
        return false;
    }
    body.statements.pop();
    true
}

fn strip_leading_comments_for_transform_flag(code: &str) -> &str {
    let bytes = code.as_bytes();
    let mut i = 0usize;
    let len = bytes.len();
    loop {
        while i < len && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 >= len {
            return &code[i..];
        }
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < len {
                i += 2;
            } else {
                return "";
            }
            continue;
        }
        return &code[i..];
    }
}
