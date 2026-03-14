//! Direct ReactiveFunction → OXC AST codegen.
//!
//! Port of `CodegenReactiveFunction.ts` from upstream. Walks the tree-shaped
//! ReactiveFunction and emits OXC AST statements with memoization cache guards,
//! replacing the intermediate string codegen + GeneratedBodyShape pipeline.

use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_span::SPAN;
use oxc_syntax::identifier::is_identifier_name;
use oxc_syntax::number::NumberBase;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator, LogicalOperator};

use crate::error::CompilerError;
use crate::hir::types::*;
use crate::reactive_scopes::build_codegen_shape::{CachePrologue, FastRefreshPrologue};

use super::codegen_reactive::{EARLY_RETURN_SENTINEL, MEMO_CACHE_SENTINEL};

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

pub struct CodegenOptions {
    pub enable_change_variable_codegen: bool,
    pub enable_emit_hook_guards: bool,
    pub enable_change_detection_for_debugging: bool,
    pub enable_reset_cache_on_source_file_changes: bool,
    pub fast_refresh_source_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

pub struct CodegenFunctionResult<'a> {
    pub body: oxc_allocator::Vec<'a, ast::Statement<'a>>,
    pub cache_size: u32,
    pub needs_cache_import: bool,
    pub param_names: Vec<String>,
    pub needs_hook_guards: bool,
    pub needs_function_hook_guard_wrapper: bool,
    pub needs_structural_check_import: bool,
    pub cache_prologue: Option<CachePrologue>,
    pub error: Option<CompilerError>,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct CodegenContext<'a> {
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    /// Identifiers that have been declared (to avoid re-declaring).
    declared: HashSet<IdentifierId>,
    /// Temp expressions: instructions whose lvalue is a temporary (used once, inlined).
    temps: HashMap<IdentifierId, Option<ast::Expression<'a>>>,
    /// Next cache slot index.
    next_cache_index: u32,
    /// Cache binding name (e.g., "$").
    cache_binding: String,
    /// Whether we emitted hook guard calls.
    emitted_hook_guards: bool,
    /// Whether we need the function-level hook guard wrapper.
    needs_function_hook_guard_wrapper: bool,
    /// Whether we emitted structural check calls.
    needs_structural_check: bool,
    /// Options controlling codegen behavior.
    options: CodegenOptions,
    /// Set of identifiers declared by reactive scopes (promoted temps).
    scope_declarations: HashSet<IdentifierId>,
}

impl<'a> CodegenContext<'a> {
    fn alloc_cache_slot(&mut self) -> u32 {
        let slot = self.next_cache_index;
        self.next_cache_index += 1;
        slot
    }

    fn cache_access(&self, index: u32) -> ast::Expression<'a> {
        ast::Expression::from(
            self.builder.member_expression_computed(
                SPAN,
                self.builder
                    .expression_identifier(SPAN, self.builder.ident(&self.cache_binding)),
                self.builder.expression_numeric_literal(
                    SPAN,
                    f64::from(index),
                    None,
                    NumberBase::Decimal,
                ),
                false,
            ),
        )
    }

    fn cache_assign(&self, index: u32, value: ast::Expression<'a>) -> ast::Expression<'a> {
        self.builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                self.builder.member_expression_computed(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident(&self.cache_binding)),
                    self.builder.expression_numeric_literal(
                        SPAN,
                        f64::from(index),
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            value,
        )
    }

    fn sentinel_expr(&self) -> ast::Expression<'a> {
        self.builder.expression_call(
            SPAN,
            ast::Expression::from(
                self.builder.member_expression_static(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident("Symbol")),
                    self.builder.identifier_name(SPAN, "for"),
                    false,
                ),
            ),
            NONE,
            self.builder
                .vec1(ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(MEMO_CACHE_SENTINEL),
                    None,
                ))),
            false,
        )
    }

    fn early_return_sentinel_expr(&self) -> ast::Expression<'a> {
        self.builder.expression_call(
            SPAN,
            ast::Expression::from(
                self.builder.member_expression_static(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident("Symbol")),
                    self.builder.identifier_name(SPAN, "for"),
                    false,
                ),
            ),
            NONE,
            self.builder
                .vec1(ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(EARLY_RETURN_SENTINEL),
                    None,
                ))),
            false,
        )
    }

    fn ident_expr(&self, name: &str) -> ast::Expression<'a> {
        self.builder
            .expression_identifier(SPAN, self.builder.ident(name))
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn codegen_reactive_function<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    func: &ReactiveFunction,
    options: CodegenOptions,
) -> CodegenFunctionResult<'a> {
    // Collect all scope declarations upfront so we know which identifiers are
    // promoted (need explicit `let` declarations) vs temporaries (inlined).
    let mut scope_declarations = HashSet::new();
    collect_scope_declarations(&func.body, &mut scope_declarations);

    let cache_binding = "$".to_string();

    let mut cx = CodegenContext {
        builder,
        allocator,
        declared: HashSet::new(),
        temps: HashMap::new(),
        next_cache_index: 0,
        cache_binding: cache_binding.clone(),
        emitted_hook_guards: false,
        needs_function_hook_guard_wrapper: false,
        needs_structural_check: false,
        options,
        scope_declarations,
    };

    // Collect param names.
    let param_names: Vec<String> = func
        .params
        .iter()
        .filter_map(|arg| match arg {
            Argument::Place(place) => place
                .identifier
                .name
                .as_ref()
                .map(|n| n.value().to_string()),
            Argument::Spread(place) => place
                .identifier
                .name
                .as_ref()
                .map(|n| n.value().to_string()),
        })
        .collect();

    // Mark params as declared.
    for arg in &func.params {
        let id = match arg {
            Argument::Place(place) => place.identifier.id,
            Argument::Spread(place) => place.identifier.id,
        };
        cx.declared.insert(id);
    }

    let mut body_stmts = codegen_block(&mut cx, &func.body);

    // Strip trailing `return;` (void return).
    if let Some(ast::Statement::ReturnStatement(ret)) = body_stmts.last()
        && ret.argument.is_none()
    {
        body_stmts.pop();
    }

    let cache_size = cx.next_cache_index;
    let needs_cache_import = cache_size > 0;

    // Build cache prologue if needed.
    let cache_prologue = if needs_cache_import {
        let fast_refresh = cx.options.fast_refresh_source_hash.as_ref().map(|hash| {
            let index = cx.next_cache_index; // Use current index for fast refresh slot
            FastRefreshPrologue {
                cache_index: index,
                hash: hash.clone(),
                index_binding_name: format!("${}", cache_binding),
            }
        });
        Some(CachePrologue {
            binding_name: cache_binding,
            size: cache_size,
            fast_refresh,
        })
    } else {
        None
    };

    let body = builder.vec_from_iter(body_stmts);

    CodegenFunctionResult {
        body,
        cache_size,
        needs_cache_import,
        param_names,
        needs_hook_guards: cx.emitted_hook_guards,
        needs_function_hook_guard_wrapper: cx.needs_function_hook_guard_wrapper,
        needs_structural_check_import: cx.needs_structural_check,
        cache_prologue,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Block dispatcher
// ---------------------------------------------------------------------------

fn codegen_block<'a>(
    cx: &mut CodegenContext<'a>,
    block: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    codegen_block_no_reset(cx, block)
}

fn codegen_block_no_reset<'a>(
    cx: &mut CodegenContext<'a>,
    block: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    let mut stmts = Vec::new();

    for stmt in block.iter() {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(s) = codegen_instruction(cx, instr) {
                    stmts.push(s);
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let label = term_stmt.label.as_ref();
                let mut terminal_stmts = codegen_terminal(cx, &term_stmt.terminal);

                // Attach label if needed.
                if let Some(label) = label
                    && !label.implicit
                    && !terminal_stmts.is_empty()
                {
                    let first = terminal_stmts.remove(0);
                    let labeled = cx.builder.statement_labeled(
                        SPAN,
                        cx.builder
                            .label_identifier(SPAN, crate_label_name(label.id)),
                        first,
                    );
                    terminal_stmts.insert(0, labeled);
                }
                stmts.extend(terminal_stmts);
            }
            ReactiveStatement::Scope(scope_block) => {
                let scope_stmts =
                    codegen_reactive_scope(cx, &scope_block.scope, &scope_block.instructions);
                stmts.extend(scope_stmts);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                // Pruned scopes: emit instructions without memoization wrapper.
                let inner = codegen_block(cx, &pruned.instructions);
                stmts.extend(inner);
            }
        }
    }

    stmts
}

// ---------------------------------------------------------------------------
// Instruction codegen
// ---------------------------------------------------------------------------

fn codegen_instruction<'a>(
    cx: &mut CodegenContext<'a>,
    instr: &ReactiveInstruction,
) -> Option<ast::Statement<'a>> {
    let expr = codegen_instruction_value(cx, &instr.value)?;

    let Some(lvalue) = &instr.lvalue else {
        // No lvalue: emit as expression statement.
        return Some(cx.builder.statement_expression(SPAN, expr));
    };

    let id = lvalue.identifier.id;
    let name = lvalue
        .identifier
        .name
        .as_ref()
        .map(|n| n.value().to_string());

    // If this is a temporary (not a scope declaration and has no user-visible name,
    // or is a compiler-generated temp), store in temp map for inlining.
    let is_scope_decl = cx.scope_declarations.contains(&id);
    let is_promoted = name.is_some() && is_scope_decl;

    if !is_promoted && !is_scope_decl {
        // Store as temp for inlining.
        cx.temps.insert(id, Some(expr));
        return None;
    }

    let name = name.unwrap_or_else(|| format!("t{}", id.0));

    if cx.declared.contains(&id) {
        // Reassignment.
        Some(
            cx.builder.statement_expression(
                SPAN,
                cx.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        cx.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                cx.builder.ident(&name),
                            ),
                    ),
                    expr,
                ),
            ),
        )
    } else {
        // First declaration.
        cx.declared.insert(id);
        let kind = ast::VariableDeclarationKind::Let;
        let pattern = cx
            .builder
            .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&name));
        Some(ast::Statement::VariableDeclaration(
            cx.builder.alloc_variable_declaration(
                SPAN,
                kind,
                cx.builder.vec1(cx.builder.variable_declarator(
                    SPAN,
                    kind,
                    pattern,
                    NONE,
                    Some(expr),
                    false,
                )),
                false,
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// Instruction value → Expression
// ---------------------------------------------------------------------------

fn codegen_instruction_value<'a>(
    cx: &mut CodegenContext<'a>,
    value: &InstructionValue,
) -> Option<ast::Expression<'a>> {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            codegen_place(cx, place)
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => codegen_place(cx, value),
        InstructionValue::LoadGlobal { binding, .. } => Some(cx.ident_expr(binding.name())),
        InstructionValue::Primitive { value: prim, .. } => Some(lower_primitive(cx.builder, prim)),
        InstructionValue::BinaryExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_binary(
            SPAN,
            codegen_place(cx, left)?,
            lower_binary_operator(*operator),
            codegen_place(cx, right)?,
        )),
        InstructionValue::UnaryExpression {
            operator, value, ..
        } => Some(cx.builder.expression_unary(
            SPAN,
            lower_unary_operator(*operator),
            codegen_place(cx, value)?,
        )),
        InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_logical(
            SPAN,
            codegen_place(cx, left)?,
            lower_logical_operator(*operator),
            codegen_place(cx, right)?,
        )),
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => Some(cx.builder.expression_conditional(
            SPAN,
            codegen_place(cx, test)?,
            codegen_place(cx, consequent)?,
            codegen_place(cx, alternate)?,
        )),
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => Some(cx.builder.expression_call(
            SPAN,
            codegen_place(cx, callee)?,
            NONE,
            codegen_arguments(cx, args)?,
            *optional,
        )),
        InstructionValue::NewExpression { callee, args, .. } => Some(cx.builder.expression_new(
            SPAN,
            codegen_place(cx, callee)?,
            NONE,
            codegen_arguments(cx, args)?,
        )),
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional,
            call_optional,
            ..
        } => {
            let callee = codegen_method_call_callee(cx, receiver, property, *receiver_optional)?;
            Some(cx.builder.expression_call(
                SPAN,
                callee,
                NONE,
                codegen_arguments(cx, args)?,
                *call_optional,
            ))
        }
        InstructionValue::TypeCastExpression { value, .. } => codegen_place(cx, value),
        InstructionValue::ArrayExpression { elements, .. } => Some(
            cx.builder
                .expression_array(SPAN, codegen_array_elements(cx, elements)?),
        ),
        InstructionValue::ObjectExpression { properties, .. } => Some(
            cx.builder
                .expression_object(SPAN, codegen_object_properties(cx, properties)?),
        ),
        InstructionValue::TemplateLiteral {
            quasis, subexprs, ..
        } => {
            let mut expressions = cx.builder.vec();
            for expr in subexprs {
                expressions.push(codegen_place(cx, expr)?);
            }
            Some(
                cx.builder.expression_template_literal(
                    SPAN,
                    cx.builder
                        .vec_from_iter(quasis.iter().enumerate().map(|(index, quasi)| {
                            cx.builder.template_element(
                                SPAN,
                                ast::TemplateElementValue {
                                    raw: cx.builder.atom(&quasi.raw),
                                    cooked: quasi.cooked.as_deref().map(|c| cx.builder.atom(c)),
                                },
                                index + 1 == quasis.len(),
                                false,
                            )
                        })),
                    expressions,
                ),
            )
        }
        InstructionValue::FunctionExpression {
            name,
            lowered_func,
            expr_type,
            ..
        } => super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
            cx.builder,
            name.as_deref(),
            lowered_func,
            *expr_type,
        ),
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            // SAFETY: The closure is only used synchronously within lower_jsx_expression,
            // and cx outlives the call. We use a raw pointer to satisfy the Fn + Copy bound.
            let cx_ptr = cx as *mut CodegenContext<'a>;
            super::super::codegen_backend::hir_to_ast::lower_jsx_expression(
                cx.builder,
                tag,
                props,
                children.as_deref(),
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                &mut HashSet::new(),
            )
        }
        InstructionValue::JsxFragment { children, .. } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            super::super::codegen_backend::hir_to_ast::lower_jsx_fragment_expression(
                cx.builder,
                children,
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                &mut HashSet::new(),
            )
        }
        InstructionValue::JSXText { value, .. } => Some(cx.builder.expression_string_literal(
            SPAN,
            cx.builder.atom(value),
            None,
        )),
        InstructionValue::PropertyLoad {
            object,
            property,
            optional,
            ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            super::super::codegen_backend::hir_to_ast::lower_property_load(
                cx.builder,
                object,
                property,
                *optional,
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                &mut HashSet::new(),
            )
        }
        InstructionValue::ComputedLoad {
            object,
            property,
            optional,
            ..
        } => Some(ast::Expression::from(
            cx.builder.member_expression_computed(
                SPAN,
                codegen_place(cx, object)?,
                codegen_place(cx, property)?,
                *optional,
            ),
        )),
        InstructionValue::PropertyStore {
            object,
            property,
            value,
            ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            let target =
                super::super::codegen_backend::hir_to_ast::lower_property_assignment_target(
                    cx.builder,
                    object,
                    property,
                    |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                    &mut HashSet::new(),
                )?;
            Some(cx.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                target,
                codegen_place(cx, value)?,
            ))
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => Some(cx.builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                cx.builder.member_expression_computed(
                    SPAN,
                    codegen_place(cx, object)?,
                    codegen_place(cx, property)?,
                    false,
                ),
            )),
            codegen_place(cx, value)?,
        )),
        InstructionValue::PropertyDelete {
            object, property, ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            Some(cx.builder.expression_unary(
                SPAN,
                oxc_syntax::operator::UnaryOperator::Delete,
                super::super::codegen_backend::hir_to_ast::lower_property_load(
                    cx.builder,
                    object,
                    property,
                    false,
                    |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                    &mut HashSet::new(),
                )?,
            ))
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => Some(cx.builder.expression_unary(
            SPAN,
            oxc_syntax::operator::UnaryOperator::Delete,
            ast::Expression::from(cx.builder.member_expression_computed(
                SPAN,
                codegen_place(cx, object)?,
                codegen_place(cx, property)?,
                false,
            )),
        )),
        InstructionValue::StoreGlobal { name, value, .. } => Some(
            cx.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                ast::AssignmentTarget::from(
                    cx.builder
                        .simple_assignment_target_assignment_target_identifier(
                            SPAN,
                            cx.builder.ident(name),
                        ),
                ),
                codegen_place(cx, value)?,
            ),
        ),
        InstructionValue::PrefixUpdate {
            lvalue, operation, ..
        } => {
            let target = codegen_simple_assignment_target(cx, lvalue)?;
            Some(cx.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                true,
                target,
            ))
        }
        InstructionValue::PostfixUpdate {
            lvalue, operation, ..
        } => {
            let target = codegen_simple_assignment_target(cx, lvalue)?;
            Some(cx.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                false,
                target,
            ))
        }
        InstructionValue::MetaProperty { meta, property, .. } => {
            Some(cx.builder.expression_meta_property(
                SPAN,
                cx.builder.identifier_name(SPAN, cx.builder.ident(meta)),
                cx.builder.identifier_name(SPAN, cx.builder.ident(property)),
            ))
        }
        InstructionValue::Debugger { .. } => None, // handled as statement
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Place → Expression
// ---------------------------------------------------------------------------

fn codegen_place<'a>(cx: &mut CodegenContext<'a>, place: &Place) -> Option<ast::Expression<'a>> {
    let id = place.identifier.id;

    // Check temp map first — single-use inlined expression.
    if let Some(temp_slot) = cx.temps.get_mut(&id)
        && let Some(expr) = temp_slot.take()
    {
        return Some(expr);
    }

    // Use identifier name.
    if let Some(name) = place.identifier.name.as_ref() {
        return Some(cx.ident_expr(name.value()));
    }

    // Fallback: use temp name.
    Some(cx.ident_expr(&format!("t{}", id.0)))
}

fn codegen_simple_assignment_target<'a>(
    cx: &mut CodegenContext<'a>,
    place: &Place,
) -> Option<ast::SimpleAssignmentTarget<'a>> {
    let expr = codegen_place(cx, place)?;
    super::super::codegen_backend::hir_to_ast::expression_to_simple_assignment_target(
        cx.builder, expr,
    )
}

// ---------------------------------------------------------------------------
// Terminal → Statement(s)
// ---------------------------------------------------------------------------

fn codegen_terminal<'a>(
    cx: &mut CodegenContext<'a>,
    terminal: &ReactiveTerminal,
) -> Vec<ast::Statement<'a>> {
    match terminal {
        ReactiveTerminal::Return { value, .. } => {
            let expr = codegen_place(cx, value);
            vec![cx.builder.statement_return(SPAN, expr)]
        }
        ReactiveTerminal::Throw { value, .. } => {
            let Some(expr) = codegen_place(cx, value) else {
                return vec![];
            };
            vec![cx.builder.statement_throw(SPAN, expr)]
        }
        ReactiveTerminal::Break {
            target,
            target_kind,
            ..
        } => {
            if *target_kind == ReactiveTerminalTargetKind::Implicit {
                return vec![];
            }
            let label = if *target_kind == ReactiveTerminalTargetKind::Labeled {
                Some(cx.builder.label_identifier(SPAN, crate_label_name(*target)))
            } else {
                None
            };
            vec![cx.builder.statement_break(SPAN, label)]
        }
        ReactiveTerminal::Continue {
            target,
            target_kind,
            ..
        } => {
            if *target_kind == ReactiveTerminalTargetKind::Implicit {
                return vec![];
            }
            let label = if *target_kind == ReactiveTerminalTargetKind::Labeled {
                Some(cx.builder.label_identifier(SPAN, crate_label_name(*target)))
            } else {
                None
            };
            vec![cx.builder.statement_continue(SPAN, label)]
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            let consequent_stmts = codegen_block(cx, consequent);
            let consequent_block = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(consequent_stmts));
            let alternate_stmt = alternate.as_ref().map(|alt| {
                let alt_stmts = codegen_block(cx, alt);
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(alt_stmts))
            });
            vec![
                cx.builder
                    .statement_if(SPAN, test_expr, consequent_block, alternate_stmt),
            ]
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            let Some(discriminant) = codegen_place(cx, test) else {
                return vec![];
            };
            let mut switch_cases = cx.builder.vec();
            for case in cases {
                let test_expr = case.test.as_ref().and_then(|t| codegen_place(cx, t));
                let consequent = case
                    .block
                    .as_ref()
                    .map(|b| codegen_block(cx, b))
                    .unwrap_or_default();
                switch_cases.push(cx.builder.switch_case(
                    SPAN,
                    test_expr,
                    cx.builder.vec_from_iter(consequent),
                ));
            }
            vec![
                cx.builder
                    .statement_switch(SPAN, discriminant, switch_cases),
            ]
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_while(SPAN, test_expr, body)]
        }
        ReactiveTerminal::DoWhile {
            test, loop_block, ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_do_while(SPAN, body, test_expr)]
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            update_value,
            loop_block,
            ..
        } => {
            // Init: emit instructions from init block, take last as init expression.
            let init_stmts = codegen_block(cx, init);
            let for_init = if let Some(last) = init_stmts.last() {
                match last {
                    ast::Statement::VariableDeclaration(_) => {
                        // Clone the last statement as ForStatementInit
                        if let ast::Statement::VariableDeclaration(decl) =
                            init_stmts.into_iter().last().unwrap()
                        {
                            Some(ast::ForStatementInit::VariableDeclaration(decl))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            } else {
                None
            };

            let test_expr = codegen_place(cx, test);

            // Update expression.
            let update_expr = if let Some(uv) = update_value {
                codegen_instruction_value(cx, uv)
            } else if let Some(update_block) = update {
                let update_stmts = codegen_block(cx, update_block);
                // Extract expression from last statement.
                update_stmts.into_iter().last().and_then(|s| {
                    if let ast::Statement::ExpressionStatement(es) = s {
                        Some(es.unbox().expression)
                    } else {
                        None
                    }
                })
            } else {
                None
            };

            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![
                cx.builder
                    .statement_for(SPAN, for_init, test_expr, update_expr, body),
            ]
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            let init_stmts = codegen_block(cx, init);
            let left = init_stmts.into_iter().last().and_then(|s| {
                if let ast::Statement::VariableDeclaration(decl) = s {
                    Some(ast::ForStatementLeft::VariableDeclaration(decl))
                } else {
                    None
                }
            });
            let Some(left) = left else {
                return vec![];
            };
            let Some(right) = codegen_place(cx, test) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_for_of(SPAN, false, left, right, body)]
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            // ForIn: init block produces left-hand side + iterable.
            let init_stmts = codegen_block(cx, init);
            // The init block should have a variable declaration (left) and the block instructions contain the iterable.
            // For now, emit as a simple for-in with the init block's declarations.
            let (left, right) = if !init_stmts.is_empty() {
                let mut iter = init_stmts.into_iter();
                let first = iter.next();
                let left = first.and_then(|s| {
                    if let ast::Statement::VariableDeclaration(decl) = s {
                        Some(ast::ForStatementLeft::VariableDeclaration(decl))
                    } else {
                        None
                    }
                });
                // The right-hand side is from the test place — but ForIn doesn't have a test field.
                // For ForIn, the iterable comes from the last instruction of the init block.
                let right = iter.last().and_then(|s| {
                    if let ast::Statement::ExpressionStatement(es) = s {
                        Some(es.unbox().expression)
                    } else {
                        None
                    }
                });
                (left, right)
            } else {
                (None, None)
            };
            let (Some(left), Some(right)) = (left, right) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_for_in(SPAN, left, right, body)]
        }
        ReactiveTerminal::Label { block, .. } => codegen_block(cx, block),
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            let try_stmts = codegen_block(cx, block);
            let try_block = cx
                .builder
                .block_statement(SPAN, cx.builder.vec_from_iter(try_stmts));

            let catch_param = handler_binding.as_ref().and_then(|binding| {
                let name = binding.identifier.name.as_ref()?.value();
                Some(
                    cx.builder.catch_parameter(
                        SPAN,
                        cx.builder
                            .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name)),
                        NONE,
                    ),
                )
            });

            let catch_stmts = codegen_block(cx, handler);
            let catch_body = cx
                .builder
                .block_statement(SPAN, cx.builder.vec_from_iter(catch_stmts));
            let catch_clause = cx.builder.alloc_catch_clause(SPAN, catch_param, catch_body);

            vec![cx.builder.statement_try(
                SPAN,
                try_block,
                Some(catch_clause),
                Option::<oxc_allocator::Box<'_, ast::BlockStatement<'_>>>::None,
            )]
        }
    }
}

// ---------------------------------------------------------------------------
// Reactive scope (memoization)
// ---------------------------------------------------------------------------

fn codegen_reactive_scope<'a>(
    cx: &mut CodegenContext<'a>,
    scope: &ReactiveScope,
    instructions: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    let mut stmts = Vec::new();

    // Emit declarations for scope-declared variables (before the memoization guard).
    let decl_names: Vec<(String, IdentifierId)> = scope
        .declarations
        .iter()
        .filter_map(|(id, decl)| {
            let name = decl.identifier.name.as_ref()?.value().to_string();
            Some((name, *id))
        })
        .collect();

    for (name, id) in &decl_names {
        if !cx.declared.contains(id) {
            cx.declared.insert(*id);
            let pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name));
            stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        pattern,
                        NONE,
                        None,
                        false,
                    )),
                    false,
                ),
            ));
        }
    }

    // Also declare reassignment targets.
    for reassign in &scope.reassignments {
        let id = reassign.id;
        if !cx.declared.contains(&id) {
            cx.declared.insert(id);
            if let Some(name) = reassign.name.as_ref() {
                let pattern = cx
                    .builder
                    .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name.value()));
                stmts.push(ast::Statement::VariableDeclaration(
                    cx.builder.alloc_variable_declaration(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        cx.builder.vec1(cx.builder.variable_declarator(
                            SPAN,
                            ast::VariableDeclarationKind::Let,
                            pattern,
                            NONE,
                            None,
                            false,
                        )),
                        false,
                    ),
                ));
            }
        }
    }

    // Build dependency comparison: $[slot] !== dep_expr || ...
    let deps: Vec<(u32, ast::Expression<'a>)> = scope
        .dependencies
        .iter()
        .filter_map(|dep| {
            let slot = cx.alloc_cache_slot();
            let dep_expr = codegen_dependency_expr(cx, dep)?;
            Some((slot, dep_expr))
        })
        .collect();

    // Allocate cache slots for outputs.
    let output_slots: Vec<(String, u32)> = decl_names
        .iter()
        .map(|(name, _)| {
            let slot = cx.alloc_cache_slot();
            (name.clone(), slot)
        })
        .collect();

    let reassign_slots: Vec<(String, u32)> = scope
        .reassignments
        .iter()
        .filter_map(|reassign| {
            let name = reassign.name.as_ref()?.value().to_string();
            let slot = cx.alloc_cache_slot();
            Some((name, slot))
        })
        .collect();

    // Build the scope body.
    let body_stmts = codegen_block(cx, instructions);

    if deps.is_empty() {
        // Zero-dependency: use sentinel check.
        let sentinel_slot = if !output_slots.is_empty() {
            output_slots[0].1
        } else if !reassign_slots.is_empty() {
            reassign_slots[0].1
        } else {
            cx.alloc_cache_slot()
        };

        let test = cx.builder.expression_binary(
            SPAN,
            cx.cache_access(sentinel_slot),
            oxc_syntax::operator::BinaryOperator::StrictEquality,
            cx.sentinel_expr(),
        );

        let mut else_body = Vec::new();
        for (name, slot) in &output_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }
        for (name, slot) in &reassign_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        let mut if_body = body_stmts;
        for (name, slot) in &output_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }
        for (name, slot) in &reassign_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        let else_stmt = if else_body.is_empty() {
            None
        } else {
            Some(
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(else_body)),
            )
        };

        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(if_body)),
                else_stmt,
            ),
        );
    } else {
        // Multi-dependency: compare deps.
        // We need dep expressions twice (test + store), so clone for the test.
        let mut test_parts: Vec<ast::Expression<'a>> = Vec::new();
        for (slot, dep_expr) in &deps {
            test_parts.push(cx.builder.expression_binary(
                SPAN,
                cx.cache_access(*slot),
                oxc_syntax::operator::BinaryOperator::StrictInequality,
                dep_expr.clone_in(cx.allocator),
            ));
        }

        let test = test_parts
            .into_iter()
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        // If body: recompute + store deps + store outputs.
        let mut if_body = body_stmts;
        // Store dependency values (consume deps here since this is the last use).
        for (slot, dep_expr) in deps {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
        // Store outputs.
        for (name, slot) in &output_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }
        for (name, slot) in &reassign_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        // Else body: load cached values.
        let mut else_body = Vec::new();
        for (name, slot) in &output_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }
        for (name, slot) in &reassign_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        let else_stmt = if else_body.is_empty() {
            None
        } else {
            Some(
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(else_body)),
            )
        };

        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(if_body)),
                else_stmt,
            ),
        );
    }

    // Handle early return value.
    if let Some(early_return) = &scope.early_return_value {
        let name = early_return
            .value
            .name
            .as_ref()
            .map(|n| n.value().to_string())
            .unwrap_or_else(|| format!("t{}", early_return.value.id.0));
        let test = cx.builder.expression_binary(
            SPAN,
            cx.ident_expr(&name),
            oxc_syntax::operator::BinaryOperator::StrictInequality,
            cx.early_return_sentinel_expr(),
        );
        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_return(SPAN, Some(cx.ident_expr(&name))),
                None,
            ),
        );
    }

    stmts
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn codegen_dependency_expr<'a>(
    cx: &mut CodegenContext<'a>,
    dep: &ReactiveScopeDependency,
) -> Option<ast::Expression<'a>> {
    let name = dep.identifier.name.as_ref()?.value();
    let mut expr = cx.ident_expr(name);
    for entry in &dep.path {
        if entry.optional {
            expr = ast::Expression::from(
                cx.builder.member_expression_static(
                    SPAN,
                    expr,
                    cx.builder
                        .identifier_name(SPAN, cx.builder.ident(entry.property.as_str())),
                    true,
                ),
            );
        } else {
            expr = ast::Expression::from(
                cx.builder.member_expression_static(
                    SPAN,
                    expr,
                    cx.builder
                        .identifier_name(SPAN, cx.builder.ident(entry.property.as_str())),
                    false,
                ),
            );
        }
    }
    Some(expr)
}

fn codegen_arguments<'a>(
    cx: &mut CodegenContext<'a>,
    args: &[Argument],
) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
    let mut lowered = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Argument::Place(place) => {
                lowered.push(ast::Argument::from(codegen_place(cx, place)?));
            }
            Argument::Spread(place) => {
                lowered.push(
                    cx.builder
                        .argument_spread_element(SPAN, codegen_place(cx, place)?),
                );
            }
        }
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_method_call_callee<'a>(
    cx: &mut CodegenContext<'a>,
    receiver: &Place,
    property: &Place,
    receiver_optional: bool,
) -> Option<ast::Expression<'a>> {
    // Check if the property is a PropertyLoad on the receiver.
    let prop_id = property.identifier.id;
    if let Some(Some(_prop_expr)) = cx.temps.get(&prop_id) {
        // If it's a member expression on the same receiver, use it directly.
        // Otherwise, fall through to computed member.
    }

    // Default: computed member expression.
    Some(ast::Expression::from(
        cx.builder.member_expression_computed(
            SPAN,
            codegen_place(cx, receiver)?,
            codegen_place(cx, property)?,
            receiver_optional,
        ),
    ))
}

fn codegen_array_elements<'a>(
    cx: &mut CodegenContext<'a>,
    elements: &[ArrayElement],
) -> Option<oxc_allocator::Vec<'a, ast::ArrayExpressionElement<'a>>> {
    let mut lowered = Vec::with_capacity(elements.len());
    for element in elements {
        let el = match element {
            ArrayElement::Place(place) => {
                ast::ArrayExpressionElement::from(codegen_place(cx, place)?)
            }
            ArrayElement::Spread(place) => cx
                .builder
                .array_expression_element_spread_element(SPAN, codegen_place(cx, place)?),
            ArrayElement::Hole => cx.builder.array_expression_element_elision(SPAN),
        };
        lowered.push(el);
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_object_properties<'a>(
    cx: &mut CodegenContext<'a>,
    properties: &[ObjectPropertyOrSpread],
) -> Option<oxc_allocator::Vec<'a, ast::ObjectPropertyKind<'a>>> {
    let mut lowered = Vec::with_capacity(properties.len());
    for property in properties {
        let prop = match property {
            ObjectPropertyOrSpread::Spread(place) => cx
                .builder
                .object_property_kind_spread_property(SPAN, codegen_place(cx, place)?),
            ObjectPropertyOrSpread::Property(property) => {
                let value = codegen_place(cx, &property.place)?;
                let (key, shorthand, computed) =
                    codegen_object_property_key(cx, &property.key, &value)?;
                cx.builder.object_property_kind_object_property(
                    SPAN,
                    ast::PropertyKind::Init,
                    key,
                    value,
                    property.type_ == ObjectPropertyType::Method,
                    shorthand,
                    computed,
                )
            }
        };
        lowered.push(prop);
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_object_property_key<'a>(
    cx: &mut CodegenContext<'a>,
    key: &ObjectPropertyKey,
    value: &ast::Expression<'a>,
) -> Option<(ast::PropertyKey<'a>, bool, bool)> {
    match key {
        ObjectPropertyKey::Identifier(name) => {
            let shorthand = matches!(
                value,
                ast::Expression::Identifier(identifier) if identifier.name == name.as_str()
            );
            Some((
                cx.builder
                    .property_key_static_identifier(SPAN, cx.builder.ident(name)),
                shorthand,
                false,
            ))
        }
        ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
            false,
        )),
        ObjectPropertyKey::String(name) => Some((
            ast::PropertyKey::from(cx.builder.expression_string_literal(
                SPAN,
                cx.builder.atom(name),
                None,
            )),
            false,
            false,
        )),
        ObjectPropertyKey::Number(val) => Some((
            ast::PropertyKey::from(cx.builder.expression_numeric_literal(
                SPAN,
                *val,
                None,
                NumberBase::Decimal,
            )),
            false,
            false,
        )),
        ObjectPropertyKey::Computed(place) => Some((
            ast::PropertyKey::from(codegen_place(cx, place)?),
            false,
            true,
        )),
    }
}

fn collect_scope_declarations(block: &ReactiveBlock, out: &mut HashSet<IdentifierId>) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Scope(scope_block) => {
                for (id, _) in &scope_block.scope.declarations {
                    out.insert(*id);
                }
                for reassign in &scope_block.scope.reassignments {
                    out.insert(reassign.id);
                }
                collect_scope_declarations(&scope_block.instructions, out);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                collect_scope_declarations(&pruned.instructions, out);
            }
            ReactiveStatement::Terminal(term) => {
                collect_terminal_scope_declarations(&term.terminal, out);
            }
            ReactiveStatement::Instruction(_) => {}
        }
    }
}

fn collect_terminal_scope_declarations(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<IdentifierId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_scope_declarations(consequent, out);
            if let Some(alt) = alternate {
                collect_scope_declarations(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_scope_declarations(block, out);
                }
            }
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. } => {
            collect_scope_declarations(loop_block, out);
        }
        ReactiveTerminal::For {
            init,
            loop_block,
            update,
            ..
        } => {
            collect_scope_declarations(init, out);
            collect_scope_declarations(loop_block, out);
            if let Some(u) = update {
                collect_scope_declarations(u, out);
            }
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_scope_declarations(init, out);
            collect_scope_declarations(loop_block, out);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_scope_declarations(block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_scope_declarations(block, out);
            collect_scope_declarations(handler, out);
        }
        _ => {}
    }
}

fn crate_label_name(block_id: BlockId) -> &'static str {
    // Leak a string for the label name. In production, this would use an arena.
    let s = format!("bb{}", block_id.0);
    Box::leak(s.into_boxed_str())
}

// ---------------------------------------------------------------------------
// Operator helpers (reused from hir_to_ast)
// ---------------------------------------------------------------------------

fn lower_primitive<'a>(builder: AstBuilder<'a>, value: &PrimitiveValue) -> ast::Expression<'a> {
    super::super::codegen_backend::hir_to_ast::lower_primitive(builder, value)
}

fn lower_binary_operator(op: crate::hir::types::BinaryOperator) -> BinaryOperator {
    super::super::codegen_backend::hir_to_ast::lower_binary_operator(op)
}

fn lower_unary_operator(
    op: crate::hir::types::UnaryOperator,
) -> oxc_syntax::operator::UnaryOperator {
    super::super::codegen_backend::hir_to_ast::lower_unary_operator(op)
}

fn lower_logical_operator(op: crate::hir::types::LogicalOperator) -> LogicalOperator {
    super::super::codegen_backend::hir_to_ast::lower_logical_operator(op)
}

fn lower_update_operator(
    op: crate::hir::types::UpdateOperator,
) -> oxc_syntax::operator::UpdateOperator {
    super::super::codegen_backend::hir_to_ast::lower_update_operator(op)
}
