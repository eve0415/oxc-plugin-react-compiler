use std::collections::HashSet;

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_ast_visit::{Visit, VisitMut, walk, walk_mut};
use oxc_span::{SPAN, SourceType};
use oxc_syntax::number::NumberBase;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator, LogicalOperator};

use super::flow_cast::parse_expression_source;
use super::function_replacement::build_const_binding_statement;
use super::{AstRenderState, RenderedOutlinedFunction};
use crate::codegen_backend::{CompiledFunction, SynthesizedDefaultParamCache};

pub(super) fn apply_preserved_directives<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    directives: &[String],
) {
    if directives.is_empty() {
        return;
    }
    body.directives = builder.vec_from_iter(
        directives
            .iter()
            .filter_map(|directive| build_directive(builder, directive)),
    );
}

fn build_directive<'a>(builder: AstBuilder<'a>, directive: &str) -> Option<ast::Directive<'a>> {
    let value = parse_directive_literal_value(directive)?;
    Some(builder.directive(
        SPAN,
        builder.string_literal(SPAN, builder.atom(value), None),
        builder.atom(value),
    ))
}

fn parse_directive_literal_value(directive: &str) -> Option<&str> {
    let directive = directive.trim();
    if directive.len() < 2 {
        return None;
    }
    let quote = directive.chars().next()?;
    if !matches!(quote, '"' | '\'') || !directive.ends_with(quote) {
        return None;
    }
    Some(&directive[quote.len_utf8()..directive.len() - quote.len_utf8()])
}

pub(super) fn prepend_cache_prologue_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cache_prologue: Option<&crate::codegen_backend::codegen_ast::CachePrologue>,
    state: &AstRenderState,
) {
    let Some(cache_prologue) = cache_prologue else {
        return;
    };

    let mut statements = builder.vec1(build_cache_initializer_statement(
        builder,
        &cache_prologue.binding_name,
        cache_prologue.size,
        &state.cache_import_name,
    ));
    if let Some(fast_refresh) = cache_prologue.fast_refresh.as_ref() {
        statements.push(build_fast_refresh_reset_statement(
            builder,
            &cache_prologue.binding_name,
            cache_prologue.size,
            fast_refresh,
        ));
    }
    statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
}

pub(super) fn prepend_synthesized_default_param_cache_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
) -> Option<()> {
    let Some(default_cache) = cf.synthesized_default_param_cache.as_ref() else {
        return Some(());
    };
    let cache_binding_name = cf
        .cache_prologue
        .as_ref()
        .map(|cache_prologue| cache_prologue.binding_name.as_str())
        .unwrap_or("$");
    let insert_idx = cf.cache_prologue.as_ref().map_or(0, |cache_prologue| {
        1 + usize::from(cache_prologue.fast_refresh.is_some())
    });
    let mut statements = builder.vec();
    statements.extend(
        body.statements[..insert_idx]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    statements.extend(build_synthesized_default_param_cache_statements(
        builder,
        allocator,
        source_type,
        cache_binding_name,
        default_cache,
    )?);
    statements.extend(
        body.statements[insert_idx..]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
    Some(())
}

fn build_synthesized_default_param_cache_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cache_binding_name: &str,
    default_cache: &SynthesizedDefaultParamCache,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let value_expr =
        parse_expression_source(allocator, source_type, &default_cache.value_expr).ok()?;
    let undefined_check = builder.expression_binary(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
        BinaryOperator::StrictEquality,
        builder.expression_identifier(SPAN, builder.ident("undefined")),
    );
    let assign_value = build_identifier_assignment_statement(
        builder,
        &default_cache.value_name,
        builder.expression_conditional(
            SPAN,
            undefined_check,
            value_expr,
            builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
        ),
    );
    let else_block = builder.statement_block(
        SPAN,
        builder.vec1(build_identifier_assignment_statement(
            builder,
            &default_cache.value_name,
            cache_member_slot_expression(builder, cache_binding_name, 1),
        )),
    );

    Some(builder.vec_from_array([
        build_let_declaration_statement(builder, &default_cache.value_name),
        builder.statement_if(
            SPAN,
            builder.expression_binary(
                SPAN,
                cache_member_slot_expression(builder, cache_binding_name, 0),
                BinaryOperator::StrictInequality,
                builder.expression_identifier(SPAN, builder.ident(&default_cache.temp_name)),
            ),
            builder.statement_block(
                SPAN,
                builder.vec_from_array(
                    [
                        assign_value,
                        build_cache_slot_assignment_statement(
                            builder,
                            cache_binding_name,
                            0,
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&default_cache.temp_name),
                            ),
                        ),
                        build_cache_slot_assignment_statement(
                            builder,
                            cache_binding_name,
                            1,
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&default_cache.value_name),
                            ),
                        ),
                    ],
                ),
            ),
            Some(else_block),
        ),
    ]))
}

pub(super) fn wrap_function_hook_guard_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    wrap_hook_guard_body(
        builder,
        allocator,
        body,
        cf.needs_function_hook_guard_wrapper,
        state,
    );
}

pub(super) fn wrap_hook_guard_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    needs_function_hook_guard_wrapper: bool,
    state: &AstRenderState,
) {
    if !needs_function_hook_guard_wrapper || state.hook_guard_ident.is_empty() {
        return;
    }

    let mut try_statements = builder.vec1(build_hook_guard_statement(
        builder,
        &state.hook_guard_ident,
        crate::codegen_backend::codegen_ast::HOOK_GUARD_PUSH,
    ));
    try_statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    let finalizer = builder.alloc_block_statement(
        SPAN,
        builder.vec1(build_hook_guard_statement(
            builder,
            &state.hook_guard_ident,
            crate::codegen_backend::codegen_ast::HOOK_GUARD_POP,
        )),
    );

    body.statements = builder.vec1(ast::Statement::TryStatement(builder.alloc_try_statement(
        SPAN,
        builder.alloc_block_statement(SPAN, try_statements),
        None::<oxc_allocator::Box<'a, ast::CatchClause<'a>>>,
        Some(finalizer),
    )));
}

fn build_hook_guard_statement<'a>(
    builder: AstBuilder<'a>,
    hook_guard_ident: &str,
    action: u8,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(hook_guard_ident)),
            NONE,
            builder.vec1(ast::Argument::from(builder.expression_numeric_literal(
                SPAN,
                action as f64,
                None,
                NumberBase::Decimal,
            ))),
            false,
        ),
    )
}

fn build_let_declaration_statement<'a>(builder: AstBuilder<'a>, name: &str) -> ast::Statement<'a> {
    let pattern = builder.binding_pattern_binding_identifier(SPAN, builder.ident(name));
    ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        SPAN,
        ast::VariableDeclarationKind::Let,
        builder.vec1(builder.variable_declarator(
            SPAN,
            ast::VariableDeclarationKind::Let,
            pattern,
            NONE,
            None,
            false,
        )),
        false,
    ))
}

fn build_identifier_assignment_statement<'a>(
    builder: AstBuilder<'a>,
    name: &str,
    value: ast::Expression<'a>,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(
                builder.simple_assignment_target_assignment_target_identifier(
                    SPAN,
                    builder.ident(name),
                ),
            ),
            value,
        ),
    )
}

fn build_cache_slot_assignment_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    slot: u32,
    value: ast::Expression<'a>,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                builder.member_expression_computed(
                    SPAN,
                    builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                    builder.expression_numeric_literal(
                        SPAN,
                        slot as f64,
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            value,
        ),
    )
}

pub(super) fn normalize_use_fire_binding_temps_ast<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
) {
    if !cf.normalize_use_fire_binding_temps {
        return;
    }

    let mut declared_names = Vec::new();
    let mut collector = UseFireBindingCollector {
        declared_names: &mut declared_names,
    };
    collector.visit_function_body(body);
    if declared_names.len() < 2 {
        return;
    }

    let mut desired = declared_names.clone();
    desired.sort_by_key(|name| parse_temp_token_index(name).unwrap_or(u32::MAX));
    if desired == declared_names {
        return;
    }

    let renames = declared_names
        .iter()
        .zip(desired.iter())
        .filter_map(|(from, to)| (from != to).then_some((from.as_str(), to.as_str())))
        .collect::<Vec<_>>();
    if renames.is_empty() {
        return;
    }

    let mut renamer = BindingAndReferenceRenamer { builder, renames };
    renamer.visit_function_body(body);
}

fn parse_temp_token_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix('t')?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok()
}

pub(super) fn prepend_compiled_body_prefix_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    original_body: Option<&ast::FunctionBody<'_>>,
    cache_import_name: Option<&str>,
) -> Option<()> {
    let prefix_statements =
        super::collect_compiled_body_prefix_statements(allocator, source_type, body, cf)?;
    let preserved_original_statements =
        super::collect_preserved_original_body_statements(allocator, original_body);
    if prefix_statements.is_empty() && preserved_original_statements.is_empty() {
        return Some(());
    }
    let insert_idx = cache_import_name
        .and_then(|cache_import_name| {
            super::find_cache_initializer_index(&body.statements, cache_import_name)
        })
        .map_or(0, |index| index + 1);
    let mut statements = builder.vec();
    statements.extend(
        body.statements[..insert_idx]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    statements.extend(prefix_statements);
    statements.extend(preserved_original_statements);
    statements.extend(
        body.statements[insert_idx..]
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
    Some(())
}

fn build_cache_initializer_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    cache_size: u32,
    cache_import_name: &str,
) -> ast::Statement<'a> {
    build_const_binding_statement(
        builder,
        SPAN,
        cache_binding_name,
        builder.expression_call(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(cache_import_name)),
            NONE,
            builder.vec1(ast::Argument::from(builder.expression_numeric_literal(
                SPAN,
                cache_size as f64,
                None,
                NumberBase::Decimal,
            ))),
            false,
        ),
    )
}

fn build_fast_refresh_reset_statement<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    cache_size: u32,
    fast_refresh: &crate::codegen_backend::codegen_ast::FastRefreshPrologue,
) -> ast::Statement<'a> {
    let test = builder.expression_binary(
        SPAN,
        cache_member_slot_expression(builder, cache_binding_name, fast_refresh.cache_index),
        BinaryOperator::StrictInequality,
        builder.expression_string_literal(SPAN, builder.atom(&fast_refresh.hash), None),
    );
    let mut consequent_statements = builder.vec1(builder.statement_for(
        SPAN,
        Some(ast::ForStatementInit::VariableDeclaration(
            builder.alloc_variable_declaration(
                SPAN,
                ast::VariableDeclarationKind::Let,
                builder.vec1(builder.variable_declarator(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    builder.binding_pattern_binding_identifier(
                        SPAN,
                        builder.ident(&fast_refresh.index_binding_name),
                    ),
                    NONE,
                    Some(builder.expression_numeric_literal(SPAN, 0.0, None, NumberBase::Decimal)),
                    false,
                )),
                false,
            ),
        )),
        Some(builder.expression_binary(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident(&fast_refresh.index_binding_name)),
            BinaryOperator::LessThan,
            builder.expression_numeric_literal(SPAN, cache_size as f64, None, NumberBase::Decimal),
        )),
        Some(builder.expression_assignment(
            SPAN,
            AssignmentOperator::Addition,
            ast::AssignmentTarget::from(
                builder.simple_assignment_target_assignment_target_identifier(
                    SPAN,
                    builder.ident(&fast_refresh.index_binding_name),
                ),
            ),
            builder.expression_numeric_literal(SPAN, 1.0, None, NumberBase::Decimal),
        )),
        builder.statement_block(
            SPAN,
            builder.vec1(builder.statement_expression(
                SPAN,
                builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                        builder.member_expression_computed(
                            SPAN,
                            builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                            builder.expression_identifier(
                                SPAN,
                                builder.ident(&fast_refresh.index_binding_name),
                            ),
                            false,
                        ),
                    )),
                    build_memo_cache_sentinel_expression(builder),
                ),
            )),
        ),
    ));
    consequent_statements.push(builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                builder.member_expression_computed(
                    SPAN,
                    builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
                    builder.expression_numeric_literal(
                        SPAN,
                        fast_refresh.cache_index as f64,
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            builder.expression_string_literal(SPAN, builder.atom(&fast_refresh.hash), None),
        ),
    ));
    builder.statement_if(
        SPAN,
        test,
        builder.statement_block(SPAN, consequent_statements),
        None,
    )
}

fn cache_member_slot_expression<'a>(
    builder: AstBuilder<'a>,
    cache_binding_name: &str,
    slot: u32,
) -> ast::Expression<'a> {
    ast::Expression::from(builder.member_expression_computed(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(cache_binding_name)),
        builder.expression_numeric_literal(SPAN, slot as f64, None, NumberBase::Decimal),
        false,
    ))
}

fn build_memo_cache_sentinel_expression<'a>(builder: AstBuilder<'a>) -> ast::Expression<'a> {
    builder.expression_call(
        SPAN,
        ast::Expression::from(builder.member_expression_static(
            SPAN,
            builder.expression_identifier(SPAN, builder.ident("Symbol")),
            builder.identifier_name(SPAN, "for"),
            false,
        )),
        NONE,
        builder.vec1(ast::Argument::from(builder.expression_string_literal(
            SPAN,
            builder.atom(crate::codegen_backend::codegen_ast::MEMO_CACHE_SENTINEL),
            None,
        ))),
        false,
    )
}

pub(super) fn prepend_instrument_forget_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    if !cf.needs_instrument_forget {
        return;
    }

    let rendered_name = if cf.name.is_empty() {
        "<anonymous>"
    } else {
        cf.name.as_str()
    };
    let test = builder.expression_logical(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident("DEV")),
        LogicalOperator::And,
        builder.expression_identifier(SPAN, builder.ident(&state.should_instrument_ident)),
    );
    let call = builder.expression_call(
        SPAN,
        builder.expression_identifier(SPAN, builder.ident(&state.use_render_counter_ident)),
        NONE,
        builder.vec_from_iter([
            ast::Argument::from(builder.expression_string_literal(
                SPAN,
                builder.atom(rendered_name),
                None,
            )),
            ast::Argument::from(builder.expression_string_literal(
                SPAN,
                builder.atom(&state.instrument_source_path),
                None,
            )),
        ]),
        false,
    );
    let statement =
        builder.statement_if(SPAN, test, builder.statement_expression(SPAN, call), None);

    let mut statements = builder.vec1(statement);
    statements.extend(
        body.statements
            .iter()
            .map(|statement| statement.clone_in(allocator)),
    );
    body.statements = statements;
}

pub(super) fn collect_rendered_outlined_functions(
    cf: &CompiledFunction,
) -> Vec<RenderedOutlinedFunction> {
    cf.outlined_functions
        .iter()
        .map(|outlined_function| RenderedOutlinedFunction {
            name: outlined_function.name.clone(),
            params: outlined_function.params.clone(),
            directives: outlined_function.directives.clone(),
            cache_prologue: outlined_function.cache_prologue.clone(),
            needs_function_hook_guard_wrapper: outlined_function.needs_function_hook_guard_wrapper,
            is_async: outlined_function.is_async,
            is_generator: outlined_function.is_generator,
            reactive_function: outlined_function.reactive_function.clone(),
            enable_change_variable_codegen: cf.enable_change_variable_codegen,
            unique_identifiers: outlined_function.unique_identifiers.clone(),
        })
        .collect()
}

pub(super) fn try_lower_compiled_statement_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    if !super::can_emit_compiled_statement_ast(cf) {
        return None;
    }
    let mut statements = builder.vec();
    let mut function_statement = super::super::hir_to_ast::try_lower_function_declaration_ast(
        builder,
        cf.hir_function.as_ref()?,
    )?;
    let ast::Statement::FunctionDeclaration(function) = &mut function_statement else {
        return None;
    };
    prepend_hir_body_prefix_statements(builder, allocator, source_type, function, cf)?;
    prepend_hir_instrument_forget_statement(builder, allocator, function, cf, state)?;
    apply_emit_freeze_to_hir_function_body(builder, allocator, function, cf, state)?;
    statements.push(function_statement);
    if !cf.outlined_functions.is_empty() {
        return None;
    }
    for (_, hir_function) in cf.hir_outlined_functions.iter().rev() {
        statements.push(
            super::super::hir_to_ast::try_lower_function_declaration_ast(builder, hir_function)?,
        );
    }
    Some(statements)
}

fn prepend_hir_body_prefix_statements<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    source_type: SourceType,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
) -> Option<()> {
    let body = function.body.as_ref()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    prepend_compiled_body_prefix_statements(
        builder,
        allocator,
        source_type,
        &mut cloned_body,
        cf,
        None,
        None,
    )?;
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

fn prepend_hir_instrument_forget_statement<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<()> {
    let body = function.body.as_mut()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    prepend_instrument_forget_statement(builder, allocator, &mut cloned_body, cf, state);
    align_runtime_identifier_references(builder, &mut cloned_body, cf, state);
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

pub(super) fn apply_emit_freeze_to_hir_function_body<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    function: &mut ast::Function<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) -> Option<()> {
    let body = function.body.as_mut()?;
    let mut cloned_body = body.clone_in(allocator).unbox();
    apply_emit_freeze_to_cache_stores_ast(builder, allocator, &mut cloned_body, cf, state);
    function.body = Some(builder.alloc(cloned_body));
    Some(())
}

pub(super) fn align_runtime_identifier_references<'a>(
    builder: AstBuilder<'a>,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    let mut renames = Vec::new();
    if cf.needs_hook_guards && !state.hook_guard_ident.is_empty() {
        renames.push(("$dispatcherGuard", state.hook_guard_ident.as_str()));
    }
    if cf.needs_structural_check_import && !state.structural_check_ident.is_empty() {
        renames.push(("$structuralCheck", state.structural_check_ident.as_str()));
    }
    if cf.needs_lower_context_access
        && !state.lower_context_access_imported.is_empty()
        && !state.lower_context_access_ident.is_empty()
        && state.lower_context_access_imported != state.lower_context_access_ident
    {
        renames.push((
            state.lower_context_access_imported.as_str(),
            state.lower_context_access_ident.as_str(),
        ));
    }
    let cache_import_name =
        (state.cache_import_name != "_c").then_some(state.cache_import_name.as_str());
    if renames.is_empty() && cache_import_name.is_none() {
        return;
    }

    let mut renamer = IdentifierReferenceRenamer {
        builder,
        cache_import_name,
        renames,
    };
    renamer.visit_function_body(body);
}

pub(super) fn apply_emit_freeze_to_cache_stores_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    body: &mut ast::FunctionBody<'a>,
    cf: &CompiledFunction,
    state: &AstRenderState,
) {
    if !cf.needs_emit_freeze || state.make_read_only_ident.is_empty() || cf.name.is_empty() {
        return;
    }

    let mut collector = OutputCacheSlotCollector {
        output_slots: HashSet::new(),
    };
    collector.visit_function_body(body);
    if collector.output_slots.is_empty() {
        return;
    }

    let mut rewriter = EmitFreezeCacheStoreRewriter {
        builder,
        allocator,
        freeze_ident: state.make_read_only_ident.as_str(),
        function_name: cf.name.as_str(),
        output_slots: collector.output_slots,
    };
    rewriter.visit_function_body(body);
}

struct IdentifierReferenceRenamer<'a, 'rename> {
    builder: AstBuilder<'a>,
    cache_import_name: Option<&'rename str>,
    renames: Vec<(&'rename str, &'rename str)>,
}

struct BindingAndReferenceRenamer<'a, 'rename> {
    builder: AstBuilder<'a>,
    renames: Vec<(&'rename str, &'rename str)>,
}

struct OutputCacheSlotCollector {
    output_slots: HashSet<(String, u32)>,
}

struct UseFireBindingCollector<'a> {
    declared_names: &'a mut Vec<String>,
}

struct EmitFreezeCacheStoreRewriter<'a, 'rename> {
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    freeze_ident: &'rename str,
    function_name: &'rename str,
    output_slots: HashSet<(String, u32)>,
}

impl<'a> VisitMut<'a> for IdentifierReferenceRenamer<'a, '_> {
    fn visit_call_expression(&mut self, it: &mut ast::CallExpression<'a>) {
        if let Some(cache_import_name) = self.cache_import_name
            && let ast::Expression::Identifier(identifier) = &mut it.callee
            && identifier.name == "_c"
        {
            identifier.name = self.builder.ident(cache_import_name);
        }
        walk_mut::walk_call_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &mut ast::IdentifierReference<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }
}

impl<'a> VisitMut<'a> for BindingAndReferenceRenamer<'a, '_> {
    fn visit_binding_identifier(&mut self, it: &mut ast::BindingIdentifier<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }

    fn visit_identifier_reference(&mut self, it: &mut ast::IdentifierReference<'a>) {
        if let Some((_, to)) = self
            .renames
            .iter()
            .find(|(from, _)| it.name.as_str() == *from)
        {
            it.name = self.builder.ident(to);
        }
    }
}

impl<'a> Visit<'a> for UseFireBindingCollector<'_> {
    fn visit_variable_declarator(&mut self, it: &ast::VariableDeclarator<'a>) {
        if let Some(ast::Expression::CallExpression(call)) = &it.init
            && matches!(&call.callee, ast::Expression::Identifier(identifier) if identifier.name == "useFire")
            && let ast::BindingPattern::BindingIdentifier(ident) = &it.id
            && parse_temp_token_index(ident.name.as_str()).is_some()
            && !self
                .declared_names
                .iter()
                .any(|existing| existing == ident.name.as_str())
        {
            self.declared_names.push(ident.name.to_string());
        }
        walk::walk_variable_declarator(self, it);
    }
}

impl<'a> Visit<'a> for OutputCacheSlotCollector {
    fn visit_assignment_expression(&mut self, it: &ast::AssignmentExpression<'a>) {
        if it.operator == AssignmentOperator::Assign
            && let Some(_) = assignment_target_identifier_name(&it.left)
            && let Some((cache_name, slot)) = expression_cache_access(&it.right)
        {
            self.output_slots.insert((cache_name.to_string(), slot));
        }
        walk::walk_assignment_expression(self, it);
    }
}

impl<'a> VisitMut<'a> for EmitFreezeCacheStoreRewriter<'a, '_> {
    fn visit_assignment_expression(&mut self, it: &mut ast::AssignmentExpression<'a>) {
        walk_mut::walk_assignment_expression(self, it);
        if it.operator != AssignmentOperator::Assign {
            return;
        }
        let Some((cache_name, slot)) = assignment_target_cache_access(&it.left) else {
            return;
        };
        if !self.output_slots.contains(&(cache_name.to_string(), slot))
            || expression_references_identifier(&it.right, self.freeze_ident)
        {
            return;
        }

        let original_right = it.right.clone_in(self.allocator);
        let freeze_call = self.builder.expression_call(
            SPAN,
            self.builder
                .expression_identifier(SPAN, self.builder.ident(self.freeze_ident)),
            NONE,
            self.builder.vec_from_iter([
                ast::Argument::from(original_right.clone_in(self.allocator)),
                ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(self.function_name),
                    None,
                )),
            ]),
            false,
        );
        it.right = self.builder.expression_conditional(
            SPAN,
            self.builder
                .expression_identifier(SPAN, self.builder.ident("__DEV__")),
            freeze_call,
            original_right,
        );
    }
}

fn assignment_target_identifier_name<'a>(target: &'a ast::AssignmentTarget<'a>) -> Option<&'a str> {
    match target {
        ast::AssignmentTarget::AssignmentTargetIdentifier(identifier) => {
            Some(identifier.name.as_str())
        }
        _ => None,
    }
}

fn assignment_target_cache_access<'a>(
    target: &'a ast::AssignmentTarget<'a>,
) -> Option<(&'a str, u32)> {
    match target {
        ast::AssignmentTarget::ComputedMemberExpression(member) => member_cache_access(member),
        _ => None,
    }
}

fn expression_cache_access<'a>(expression: &'a ast::Expression<'a>) -> Option<(&'a str, u32)> {
    match expression {
        ast::Expression::ComputedMemberExpression(member) => member_cache_access(member),
        _ => None,
    }
}

fn member_cache_access<'a>(
    member: &'a ast::ComputedMemberExpression<'a>,
) -> Option<(&'a str, u32)> {
    let ast::Expression::Identifier(object) = &member.object else {
        return None;
    };
    let ast::Expression::NumericLiteral(slot) = &member.expression else {
        return None;
    };
    let value = slot.value;
    if value.fract() != 0.0 || value < 0.0 || value > u32::MAX as f64 {
        return None;
    }
    Some((object.name.as_str(), value as u32))
}

pub(super) fn expression_references_identifier(
    expression: &ast::Expression<'_>,
    ident: &str,
) -> bool {
    let mut detector = IdentifierReferenceDetector {
        ident,
        found: false,
    };
    detector.visit_expression(expression);
    detector.found
}

struct IdentifierReferenceDetector<'ident> {
    ident: &'ident str,
    found: bool,
}

impl<'a> Visit<'a> for IdentifierReferenceDetector<'_> {
    fn visit_identifier_reference(&mut self, it: &ast::IdentifierReference<'a>) {
        if it.name == self.ident {
            self.found = true;
        }
    }
}

pub(super) fn function_body_contains_undefined_fallback(
    statements: &oxc_allocator::Vec<'_, ast::Statement<'_>>,
) -> bool {
    let mut detector = UndefinedFallbackDetector { found: false };
    detector.visit_statements(statements);
    detector.found
}

struct UndefinedFallbackDetector {
    found: bool,
}

impl<'a> Visit<'a> for UndefinedFallbackDetector {
    fn visit_expression(&mut self, expression: &ast::Expression<'a>) {
        if self.found {
            return;
        }
        if let ast::Expression::ConditionalExpression(conditional) = expression
            && conditional_expression_is_undefined_fallback(conditional)
        {
            self.found = true;
            return;
        }
        walk::walk_expression(self, expression);
    }
}

pub(super) fn conditional_expression_is_undefined_fallback(
    conditional: &ast::ConditionalExpression<'_>,
) -> bool {
    fn compared_identifier_name<'a>(expression: &'a ast::Expression<'a>) -> Option<&'a str> {
        let ast::Expression::BinaryExpression(binary) = expression.without_parentheses() else {
            return None;
        };
        if binary.operator != BinaryOperator::StrictEquality {
            return None;
        }
        if expression_is_undefined(&binary.right) {
            return identifier_reference_name(&binary.left);
        }
        if expression_is_undefined(&binary.left) {
            return identifier_reference_name(&binary.right);
        }
        None
    }

    fn identifier_reference_name<'a>(expression: &'a ast::Expression<'a>) -> Option<&'a str> {
        let ast::Expression::Identifier(identifier) = expression.without_parentheses() else {
            return None;
        };
        Some(identifier.name.as_str())
    }

    fn expression_is_undefined(expression: &ast::Expression<'_>) -> bool {
        matches!(
            expression.without_parentheses(),
            ast::Expression::Identifier(identifier) if identifier.name == "undefined"
        )
    }

    let Some(ident) = compared_identifier_name(&conditional.test) else {
        return false;
    };
    expression_references_identifier(&conditional.consequent, ident)
        || expression_references_identifier(&conditional.alternate, ident)
}
