use oxc_allocator::{Allocator, CloneIn, Dummy};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_span::GetSpan;

use super::AstRenderState;
use crate::codegen_backend::{CompiledFunction, CompiledParam};

pub(super) fn try_build_gated_function_declaration_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source: &str,
    stmt: &ast::Statement<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if cf.name.is_empty() {
        return None;
    }
    if source[stmt.span().start as usize..stmt.span().end as usize].contains("FIXTURE_ENTRYPOINT") {
        return None;
    }
    let referenced_before_decl = crate::pipeline::has_early_binding_reference(
        &source[..stmt.span().start as usize],
        &cf.name,
    );

    let function_body = super::build_compiled_function_body(
        builder,
        allocator,
        state.source_type,
        cf,
        state,
        super::find_original_compiled_function_body(stmt, cf),
    )?;
    let compiled_params = cf.compiled_params.as_deref()?;

    match stmt {
        ast::Statement::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            if referenced_before_decl {
                return build_early_reference_gated_function_declaration_statements(
                    builder,
                    allocator,
                    function,
                    gate_name,
                    cf,
                    compiled_params,
                    &function_body,
                );
            }
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            Some(builder.vec1(build_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            )))
        }
        ast::Statement::ExportNamedDeclaration(export_named)
            if matches!(
                export_named.declaration.as_ref(),
                Some(ast::Declaration::FunctionDeclaration(function))
                    if function.span.start == cf.start && function.span.end == cf.end
            ) =>
        {
            let ast::Declaration::FunctionDeclaration(function) =
                export_named.declaration.as_ref().unwrap()
            else {
                unreachable!();
            };
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            Some(builder.vec1(build_exported_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            )))
        }
        ast::Statement::ExportDefaultDeclaration(export_default)
            if matches!(
                &export_default.declaration,
                ast::ExportDefaultDeclarationKind::FunctionDeclaration(function)
                    if function.span.start == cf.start && function.span.end == cf.end
            ) =>
        {
            let ast::ExportDefaultDeclarationKind::FunctionDeclaration(function) =
                &export_default.declaration
            else {
                unreachable!();
            };
            let init = build_gated_function_declaration_initializer(
                builder,
                allocator,
                function,
                gate_name,
                cf,
                compiled_params,
                &function_body,
            )?;
            let mut statements = builder.vec();
            statements.push(build_const_function_statement(
                builder,
                stmt.span(),
                &cf.name,
                init,
            ));
            statements.push(build_export_default_identifier_statement(
                builder,
                stmt.span(),
                &cf.name,
            ));
            Some(statements)
        }
        _ => None,
    }
}

fn build_gated_function_declaration_initializer<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> Option<ast::Expression<'a>> {
    let original = function_declaration_to_expression(builder, allocator, function);
    let mut optimized = original.clone_in(allocator);
    if !replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut optimized,
        cf,
        compiled_params,
        function_body,
    ) {
        return None;
    }
    Some(make_gate_conditional_expression(
        builder,
        gate_name,
        function.span,
        optimized,
        original,
    ))
}

fn build_early_reference_gated_function_declaration_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let gate_result_name = format!("{}_result", gate_name);
    let optimized_name = format!("{}_optimized", cf.name);
    let unoptimized_name = format!("{}_unoptimized", cf.name);
    let param_count = compiled_params.len();
    let wrapper_args = (0..param_count)
        .map(|i| format!("arg{i}"))
        .collect::<Vec<_>>();

    let mut statements = builder.vec();
    statements.push(build_const_binding_statement(
        builder,
        function.span,
        &gate_result_name,
        build_identifier_call_expression(builder, function.span, gate_name, &[]),
    ));
    statements.push(build_renamed_function_declaration_statement(
        builder,
        allocator,
        function,
        &optimized_name,
        Some((compiled_params, function_body)),
        true,
    ));
    statements.push(build_renamed_function_declaration_statement(
        builder,
        allocator,
        function,
        &unoptimized_name,
        None,
        false,
    ));
    statements.push(build_gate_wrapper_function_statement(
        builder,
        function.span,
        &cf.name,
        &gate_result_name,
        &optimized_name,
        &unoptimized_name,
        &wrapper_args,
    ));
    Some(statements)
}

fn function_declaration_to_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
) -> ast::Expression<'a> {
    let mut cloned = function.clone_in(allocator);
    cloned.r#type = ast::FunctionType::FunctionExpression;
    super::strip_compiled_function_signature_types(&mut cloned);
    ast::Expression::FunctionExpression(builder.alloc(cloned))
}

fn build_const_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    build_const_binding_statement(builder, span, name, init)
}

pub(super) fn build_const_binding_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(name));
    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        span,
        ast::VariableDeclarationKind::Const,
        builder.vec1(builder.variable_declarator(
            span,
            ast::VariableDeclarationKind::Const,
            pattern,
            NONE,
            Some(init),
            false,
        )),
        false,
    ))
}

fn build_exported_const_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    init: ast::Expression<'a>,
) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(name));
    let declaration = ast::Declaration::VariableDeclaration(builder.alloc_variable_declaration(
        span,
        ast::VariableDeclarationKind::Const,
        builder.vec1(builder.variable_declarator(
            span,
            ast::VariableDeclarationKind::Const,
            pattern,
            NONE,
            Some(init),
            false,
        )),
        false,
    ));
    ast::Statement::ExportNamedDeclaration(builder.alloc_export_named_declaration(
        span,
        Some(declaration),
        builder.vec(),
        None,
        ast::ImportOrExportKind::Value,
        NONE,
    ))
}

fn build_export_default_identifier_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
) -> ast::Statement<'a> {
    ast::Statement::ExportDefaultDeclaration(builder.alloc_export_default_declaration(
        span,
        ast::ExportDefaultDeclarationKind::Identifier(
            builder.alloc_identifier_reference(span, builder.atom(name)),
        ),
    ))
}

fn build_renamed_function_declaration_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &ast::Function<'_>,
    name: &str,
    optimized: Option<(&[CompiledParam], &ast::FunctionBody<'a>)>,
    strip_signature_types: bool,
) -> ast::Statement<'a> {
    let mut cloned = function.clone_in(allocator);
    cloned.id = Some(builder.binding_identifier(function.span, builder.atom(name)));
    if strip_signature_types {
        super::strip_compiled_function_signature_types(&mut cloned);
    }
    if let Some((compiled_params, function_body)) = optimized {
        cloned.params =
            super::make_compiled_formal_params(builder, cloned.params.kind, compiled_params);
        cloned.body = Some(super::make_function_body(builder, allocator, function_body));
    }
    ast::Statement::FunctionDeclaration(builder.alloc(cloned))
}

fn build_gate_wrapper_function_statement<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    gate_result_name: &str,
    optimized_name: &str,
    unoptimized_name: &str,
    wrapper_args: &[String],
) -> ast::Statement<'a> {
    let mut function = ast::Function::dummy(builder.allocator);
    function.span = span;
    function.r#type = ast::FunctionType::FunctionDeclaration;
    function.id = Some(builder.binding_identifier(span, builder.atom(name)));
    function.params = build_wrapper_formal_params(builder, span, wrapper_args);
    let test = builder.expression_identifier(span, builder.ident(gate_result_name));
    let optimized_call =
        build_identifier_call_expression(builder, span, optimized_name, wrapper_args);
    let unoptimized_call =
        build_identifier_call_expression(builder, span, unoptimized_name, wrapper_args);
    function.body = Some(builder.alloc_function_body(
        span,
        builder.vec(),
        builder.vec1(builder.statement_if(
            span,
            test,
            builder.statement_return(span, Some(optimized_call)),
            Some(builder.statement_return(span, Some(unoptimized_call))),
        )),
    ));
    ast::Statement::FunctionDeclaration(builder.alloc(function))
}

fn build_wrapper_formal_params<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    wrapper_args: &[String],
) -> oxc_allocator::Box<'a, ast::FormalParameters<'a>> {
    let items = builder.vec_from_iter(wrapper_args.iter().map(|arg| {
        let pattern = builder.binding_pattern_binding_identifier(span, builder.ident(arg));
        builder.plain_formal_parameter(span, pattern)
    }));
    builder.alloc(builder.formal_parameters(
        span,
        ast::FormalParameterKind::FormalParameter,
        items,
        NONE,
    ))
}

fn build_identifier_call_expression<'a>(
    builder: AstBuilder<'a>,
    span: oxc_span::Span,
    name: &str,
    args: &[String],
) -> ast::Expression<'a> {
    let arguments =
        builder.vec_from_iter(args.iter().map(|arg| {
            ast::Argument::from(builder.expression_identifier(span, builder.ident(arg)))
        }));
    builder.expression_call(
        span,
        builder.expression_identifier(span, builder.ident(name)),
        NONE,
        arguments,
        false,
    )
}

pub(super) fn replace_compiled_function_in_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    statement: &mut ast::Statement<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match statement {
        ast::Statement::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            super::strip_compiled_function_signature_types(function);
            function.params =
                super::make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(super::make_function_body(builder, allocator, function_body));
            true
        }
        ast::Statement::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator(
                    builder,
                    allocator,
                    declarator,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ExpressionStatement(expression_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut expression_statement.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::ReturnStatement(return_statement) => {
            return_statement.argument.as_mut().is_some_and(|argument| {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ThrowStatement(throw_statement) => replace_compiled_function_in_expression(
            builder,
            allocator,
            &mut throw_statement.argument,
            cf,
            compiled_params,
            function_body,
        ),
        ast::Statement::BlockStatement(block_statement) => {
            block_statement.body.iter_mut().any(|statement| {
                replace_compiled_function_in_statement(
                    builder,
                    allocator,
                    statement,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::IfStatement(if_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut if_statement.test,
                cf,
                compiled_params,
                function_body,
            ) || replace_compiled_function_in_statement(
                builder,
                allocator,
                &mut if_statement.consequent,
                cf,
                compiled_params,
                function_body,
            ) || if_statement.alternate.as_mut().is_some_and(|alternate| {
                replace_compiled_function_in_statement(
                    builder,
                    allocator,
                    alternate,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::LabeledStatement(labeled_statement) => {
            replace_compiled_function_in_statement(
                builder,
                allocator,
                &mut labeled_statement.body,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::SwitchStatement(switch_statement) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut switch_statement.discriminant,
                cf,
                compiled_params,
                function_body,
            ) || switch_statement.cases.iter_mut().any(|case| {
                case.test.as_mut().is_some_and(|test| {
                    replace_compiled_function_in_expression(
                        builder,
                        allocator,
                        test,
                        cf,
                        compiled_params,
                        function_body,
                    )
                }) || case.consequent.iter_mut().any(|statement| {
                    replace_compiled_function_in_statement(
                        builder,
                        allocator,
                        statement,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
            })
        }
        ast::Statement::ExportNamedDeclaration(export_named) => export_named
            .declaration
            .as_mut()
            .is_some_and(|declaration| {
                replace_compiled_function_in_declaration(
                    builder,
                    allocator,
                    declaration,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Statement::ExportDefaultDeclaration(export_default) => {
            replace_compiled_function_in_export_default(
                builder,
                allocator,
                export_default,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

pub(super) fn replace_compiled_function_in_statement_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    statement: &mut ast::Statement<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match statement {
        ast::Statement::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator_with_gate(
                    builder,
                    allocator,
                    declarator,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ExpressionStatement(expression_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut expression_statement.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::ReturnStatement(return_statement) => {
            return_statement.argument.as_mut().is_some_and(|argument| {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::ThrowStatement(throw_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut throw_statement.argument,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::BlockStatement(block_statement) => {
            block_statement.body.iter_mut().any(|statement| {
                replace_compiled_function_in_statement_with_gate(
                    builder,
                    allocator,
                    statement,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::IfStatement(if_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut if_statement.test,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || replace_compiled_function_in_statement_with_gate(
                builder,
                allocator,
                &mut if_statement.consequent,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || if_statement.alternate.as_mut().is_some_and(|alternate| {
                replace_compiled_function_in_statement_with_gate(
                    builder,
                    allocator,
                    alternate,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        ast::Statement::LabeledStatement(labeled_statement) => {
            replace_compiled_function_in_statement_with_gate(
                builder,
                allocator,
                &mut labeled_statement.body,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Statement::SwitchStatement(switch_statement) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut switch_statement.discriminant,
                gate_name,
                cf,
                compiled_params,
                function_body,
            ) || switch_statement.cases.iter_mut().any(|case| {
                case.test.as_mut().is_some_and(|test| {
                    replace_compiled_function_in_expression_with_gate(
                        builder,
                        allocator,
                        test,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                }) || case.consequent.iter_mut().any(|statement| {
                    replace_compiled_function_in_statement_with_gate(
                        builder,
                        allocator,
                        statement,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
            })
        }
        ast::Statement::ExportNamedDeclaration(export_named) => export_named
            .declaration
            .as_mut()
            .is_some_and(|declaration| {
                replace_compiled_function_in_declaration_with_gate(
                    builder,
                    allocator,
                    declaration,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Statement::ExportDefaultDeclaration(export_default) => {
            replace_compiled_function_in_export_default_with_gate(
                builder,
                allocator,
                export_default,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declaration<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declaration: &mut ast::Declaration<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match declaration {
        ast::Declaration::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            super::strip_compiled_function_signature_types(function);
            function.params =
                super::make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(super::make_function_body(builder, allocator, function_body));
            true
        }
        ast::Declaration::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator(
                    builder,
                    allocator,
                    declarator,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declaration_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declaration: &mut ast::Declaration<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match declaration {
        ast::Declaration::VariableDeclaration(variable) => {
            variable.declarations.iter_mut().any(|declarator| {
                replace_compiled_function_in_declarator_with_gate(
                    builder,
                    allocator,
                    declarator,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            })
        }
        _ => false,
    }
}

fn replace_compiled_function_in_export_default<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    export_default: &mut ast::ExportDefaultDeclaration<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match &mut export_default.declaration {
        ast::ExportDefaultDeclarationKind::FunctionDeclaration(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            super::strip_compiled_function_signature_types(function);
            function.params =
                super::make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(super::make_function_body(builder, allocator, function_body));
            true
        }
        ast::ExportDefaultDeclarationKind::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            super::strip_compiled_function_signature_types(function);
            function.params =
                super::make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(super::make_function_body(builder, allocator, function_body));
            true
        }
        ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            super::strip_compiled_arrow_signature_types(arrow);
            arrow.params =
                super::make_compiled_formal_params(builder, arrow.params.kind, compiled_params);
            arrow.expression = false;
            arrow.body = super::make_function_body(builder, allocator, function_body);
            true
        }
        ast::ExportDefaultDeclarationKind::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression(
                builder,
                allocator,
                call_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression(
                builder,
                allocator,
                assignment_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::SequenceExpression(sequence_expression) => {
            sequence_expression
                .expressions
                .iter_mut()
                .any(|expression| {
                    replace_compiled_function_in_expression(
                        builder,
                        allocator,
                        expression,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
        }
        ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression(
                builder,
                allocator,
                conditional_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression(
                builder,
                allocator,
                logical_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression(
                builder,
                allocator,
                array_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression(
                builder,
                allocator,
                object_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut type_assertion.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_export_default_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    export_default: &mut ast::ExportDefaultDeclaration<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match &mut export_default.declaration {
        ast::ExportDefaultDeclarationKind::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            let original = ast::Expression::FunctionExpression(function.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            export_default.declaration = convert_expression_to_export_default_kind(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            let original = ast::Expression::ArrowFunctionExpression(arrow.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            export_default.declaration = convert_expression_to_export_default_kind(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::ExportDefaultDeclarationKind::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression_with_gate(
                builder,
                allocator,
                call_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression_with_gate(
                builder,
                allocator,
                assignment_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::SequenceExpression(sequence_expression) => {
            sequence_expression
                .expressions
                .iter_mut()
                .any(|expression| {
                    replace_compiled_function_in_expression_with_gate(
                        builder,
                        allocator,
                        expression,
                        gate_name,
                        cf,
                        compiled_params,
                        function_body,
                    )
                })
        }
        ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression_with_gate(
                builder,
                allocator,
                conditional_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression_with_gate(
                builder,
                allocator,
                logical_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression_with_gate(
                builder,
                allocator,
                array_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression_with_gate(
                builder,
                allocator,
                object_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut type_assertion.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_declarator<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declarator: &mut ast::VariableDeclarator<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    let Some(init) = declarator.init.as_mut() else {
        return false;
    };
    replace_compiled_function_in_expression(
        builder,
        allocator,
        init,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_declarator_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    declarator: &mut ast::VariableDeclarator<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    let Some(init) = declarator.init.as_mut() else {
        return false;
    };
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        init,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match expression {
        ast::Expression::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            super::strip_compiled_function_signature_types(function);
            function.params =
                super::make_compiled_formal_params(builder, function.params.kind, compiled_params);
            function.body = Some(super::make_function_body(builder, allocator, function_body));
            true
        }
        ast::Expression::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            super::strip_compiled_arrow_signature_types(arrow);
            arrow.params =
                super::make_compiled_formal_params(builder, arrow.params.kind, compiled_params);
            arrow.expression = false;
            arrow.body = super::make_function_body(builder, allocator, function_body);
            true
        }
        ast::Expression::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression(
                builder,
                allocator,
                call_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression(
                builder,
                allocator,
                assignment_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::SequenceExpression(sequence_expression) => sequence_expression
            .expressions
            .iter_mut()
            .any(|expression| {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    expression,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Expression::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression(
                builder,
                allocator,
                conditional_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression(
                builder,
                allocator,
                logical_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression(
                builder,
                allocator,
                array_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression(
                builder,
                allocator,
                object_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut type_assertion.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    expression: &mut ast::Expression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match expression {
        ast::Expression::FunctionExpression(function)
            if function.span.start == cf.start && function.span.end == cf.end =>
        {
            let original = ast::Expression::FunctionExpression(function.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            *expression = make_gate_conditional_expression(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::Expression::ArrowFunctionExpression(arrow)
            if arrow.span.start == cf.start && arrow.span.end == cf.end =>
        {
            let original = ast::Expression::ArrowFunctionExpression(arrow.clone_in(allocator));
            let mut optimized = original.clone_in(allocator);
            if !replace_compiled_function_in_expression(
                builder,
                allocator,
                &mut optimized,
                cf,
                compiled_params,
                function_body,
            ) {
                return false;
            }
            *expression = make_gate_conditional_expression(
                builder,
                gate_name,
                original.span(),
                optimized,
                original,
            );
            true
        }
        ast::Expression::CallExpression(call_expression) => {
            replace_compiled_function_in_call_expression_with_gate(
                builder,
                allocator,
                call_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::AssignmentExpression(assignment_expression) => {
            replace_compiled_function_in_assignment_expression_with_gate(
                builder,
                allocator,
                assignment_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ParenthesizedExpression(parenthesized_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut parenthesized_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::SequenceExpression(sequence_expression) => sequence_expression
            .expressions
            .iter_mut()
            .any(|expression| {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    expression,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }),
        ast::Expression::ConditionalExpression(conditional_expression) => {
            replace_compiled_function_in_conditional_expression_with_gate(
                builder,
                allocator,
                conditional_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::LogicalExpression(logical_expression) => {
            replace_compiled_function_in_logical_expression_with_gate(
                builder,
                allocator,
                logical_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ArrayExpression(array_expression) => {
            replace_compiled_function_in_array_expression_with_gate(
                builder,
                allocator,
                array_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::ObjectExpression(object_expression) => {
            replace_compiled_function_in_object_expression_with_gate(
                builder,
                allocator,
                object_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSAsExpression(ts_as_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_as_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_satisfies_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSNonNullExpression(ts_non_null_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut ts_non_null_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut type_assertion.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        ast::Expression::TSInstantiationExpression(instantiation_expression) => {
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                &mut instantiation_expression.expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
        _ => false,
    }
}

fn replace_compiled_function_in_call_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    call_expression: &mut ast::CallExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut call_expression.callee,
        cf,
        compiled_params,
        function_body,
    ) || call_expression.arguments.iter_mut().any(|argument| {
        replace_compiled_function_in_argument(
            builder,
            allocator,
            argument,
            cf,
            compiled_params,
            function_body,
        )
    })
}

fn replace_compiled_function_in_call_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    call_expression: &mut ast::CallExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut call_expression.callee,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || call_expression.arguments.iter_mut().any(|argument| {
        replace_compiled_function_in_argument_with_gate(
            builder,
            allocator,
            argument,
            gate_name,
            cf,
            compiled_params,
            function_body,
        )
    })
}

fn replace_compiled_function_in_assignment_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    assignment_expression: &mut ast::AssignmentExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut assignment_expression.right,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_assignment_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    assignment_expression: &mut ast::AssignmentExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut assignment_expression.right,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_conditional_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    conditional_expression: &mut ast::ConditionalExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.test,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.consequent,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut conditional_expression.alternate,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_conditional_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    conditional_expression: &mut ast::ConditionalExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.test,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.consequent,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut conditional_expression.alternate,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_logical_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    logical_expression: &mut ast::LogicalExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut logical_expression.left,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression(
        builder,
        allocator,
        &mut logical_expression.right,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_logical_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    logical_expression: &mut ast::LogicalExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut logical_expression.left,
        gate_name,
        cf,
        compiled_params,
        function_body,
    ) || replace_compiled_function_in_expression_with_gate(
        builder,
        allocator,
        &mut logical_expression.right,
        gate_name,
        cf,
        compiled_params,
        function_body,
    )
}

fn replace_compiled_function_in_array_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    array_expression: &mut ast::ArrayExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    array_expression
        .elements
        .iter_mut()
        .any(|element| match element {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut spread.argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => {
                let element_expression: &mut ast::Expression<'a> =
                    unsafe { std::mem::transmute(element) };
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    element_expression,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_array_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    array_expression: &mut ast::ArrayExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    array_expression
        .elements
        .iter_mut()
        .any(|element| match element {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut spread.argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => {
                let element_expression: &mut ast::Expression<'a> =
                    unsafe { std::mem::transmute(element) };
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    element_expression,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_object_expression<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    object_expression: &mut ast::ObjectExpression<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    object_expression
        .properties
        .iter_mut()
        .any(|property| match property {
            ast::ObjectPropertyKind::ObjectProperty(property) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut property.value,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ObjectPropertyKind::SpreadProperty(spread) => {
                replace_compiled_function_in_expression(
                    builder,
                    allocator,
                    &mut spread.argument,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_object_expression_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    object_expression: &mut ast::ObjectExpression<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    object_expression
        .properties
        .iter_mut()
        .any(|property| match property {
            ast::ObjectPropertyKind::ObjectProperty(property) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut property.value,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
            ast::ObjectPropertyKind::SpreadProperty(spread) => {
                replace_compiled_function_in_expression_with_gate(
                    builder,
                    allocator,
                    &mut spread.argument,
                    gate_name,
                    cf,
                    compiled_params,
                    function_body,
                )
            }
        })
}

fn replace_compiled_function_in_argument<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    argument: &mut ast::Argument<'a>,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match argument {
        ast::Argument::SpreadElement(spread) => replace_compiled_function_in_expression(
            builder,
            allocator,
            &mut spread.argument,
            cf,
            compiled_params,
            function_body,
        ),
        _ => {
            let argument_expression: &mut ast::Expression<'a> =
                unsafe { std::mem::transmute(argument) };
            replace_compiled_function_in_expression(
                builder,
                allocator,
                argument_expression,
                cf,
                compiled_params,
                function_body,
            )
        }
    }
}

fn replace_compiled_function_in_argument_with_gate<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    argument: &mut ast::Argument<'a>,
    gate_name: &str,
    cf: &CompiledFunction,
    compiled_params: &[CompiledParam],
    function_body: &ast::FunctionBody<'a>,
) -> bool {
    match argument {
        ast::Argument::SpreadElement(spread) => replace_compiled_function_in_expression_with_gate(
            builder,
            allocator,
            &mut spread.argument,
            gate_name,
            cf,
            compiled_params,
            function_body,
        ),
        _ => {
            let argument_expression: &mut ast::Expression<'a> =
                unsafe { std::mem::transmute(argument) };
            replace_compiled_function_in_expression_with_gate(
                builder,
                allocator,
                argument_expression,
                gate_name,
                cf,
                compiled_params,
                function_body,
            )
        }
    }
}

pub(super) fn make_gate_conditional_expression<'a>(
    builder: AstBuilder<'a>,
    gate_name: &str,
    span: oxc_span::Span,
    consequent: ast::Expression<'a>,
    alternate: ast::Expression<'a>,
) -> ast::Expression<'a> {
    let gate_call = builder.expression_call(
        span,
        builder.expression_identifier(span, builder.ident(gate_name)),
        NONE,
        builder.vec(),
        false,
    );
    builder.expression_conditional(span, gate_call, consequent, alternate)
}

pub(super) fn convert_expression_to_export_default_kind<'a>(
    builder: AstBuilder<'a>,
    gate_name: &str,
    span: oxc_span::Span,
    consequent: ast::Expression<'a>,
    alternate: ast::Expression<'a>,
) -> ast::ExportDefaultDeclarationKind<'a> {
    let conditional =
        make_gate_conditional_expression(builder, gate_name, span, consequent, alternate);
    match conditional {
        ast::Expression::FunctionExpression(function) => {
            ast::ExportDefaultDeclarationKind::FunctionExpression(function)
        }
        ast::Expression::ArrowFunctionExpression(arrow) => {
            ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow)
        }
        ast::Expression::CallExpression(call) => {
            ast::ExportDefaultDeclarationKind::CallExpression(call)
        }
        ast::Expression::ConditionalExpression(conditional) => {
            ast::ExportDefaultDeclarationKind::ConditionalExpression(conditional)
        }
        ast::Expression::AssignmentExpression(assignment) => {
            ast::ExportDefaultDeclarationKind::AssignmentExpression(assignment)
        }
        ast::Expression::ParenthesizedExpression(parenthesized) => {
            ast::ExportDefaultDeclarationKind::ParenthesizedExpression(parenthesized)
        }
        ast::Expression::SequenceExpression(sequence) => {
            ast::ExportDefaultDeclarationKind::SequenceExpression(sequence)
        }
        ast::Expression::LogicalExpression(logical) => {
            ast::ExportDefaultDeclarationKind::LogicalExpression(logical)
        }
        ast::Expression::ArrayExpression(array) => {
            ast::ExportDefaultDeclarationKind::ArrayExpression(array)
        }
        ast::Expression::ObjectExpression(object) => {
            ast::ExportDefaultDeclarationKind::ObjectExpression(object)
        }
        ast::Expression::TSAsExpression(ts_as) => {
            ast::ExportDefaultDeclarationKind::TSAsExpression(ts_as)
        }
        ast::Expression::TSSatisfiesExpression(ts_satisfies) => {
            ast::ExportDefaultDeclarationKind::TSSatisfiesExpression(ts_satisfies)
        }
        ast::Expression::TSNonNullExpression(ts_non_null) => {
            ast::ExportDefaultDeclarationKind::TSNonNullExpression(ts_non_null)
        }
        ast::Expression::TSTypeAssertion(type_assertion) => {
            ast::ExportDefaultDeclarationKind::TSTypeAssertion(type_assertion)
        }
        ast::Expression::TSInstantiationExpression(instantiation) => {
            ast::ExportDefaultDeclarationKind::TSInstantiationExpression(instantiation)
        }
        other => panic!("unsupported export default gated expression: {:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::compile_to_result;

    #[test]
    fn function_replacement_declaration() {
        let result =
            compile_to_result("function Component(props) { return <div>{props.x}</div>; }");
        assert!(result.transformed, "should be transformed");
    }

    #[test]
    fn function_replacement_arrow() {
        let mut options = crate::options::PluginOptions::default();
        options.compilation_mode = crate::options::CompilationMode::All;
        let result = crate::compile(
            "test.jsx",
            "const Component = (props) => <div>{props.x}</div>;",
            &options,
        );
        assert!(!result.code.is_empty(), "output should be non-empty");
    }
}
