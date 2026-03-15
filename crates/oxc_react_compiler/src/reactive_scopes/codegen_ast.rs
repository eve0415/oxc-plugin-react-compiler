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
    /// Variable names that have been declared (to avoid duplicate `let` for same name).
    declared_names: HashSet<String>,
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
    let cache_binding = "$".to_string();

    let mut cx = CodegenContext {
        builder,
        allocator,
        declared: HashSet::new(),
        declared_names: HashSet::new(),
        temps: HashMap::new(),
        next_cache_index: 0,
        cache_binding: cache_binding.clone(),
        emitted_hook_guards: false,
        needs_function_hook_guard_wrapper: false,
        needs_structural_check: false,
        options,
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
        let (id, name) = match arg {
            Argument::Place(place) => (place.identifier.id, &place.identifier.name),
            Argument::Spread(place) => (place.identifier.id, &place.identifier.name),
        };
        cx.declared.insert(id);
        if let Some(n) = name {
            cx.declared_names.insert(n.value().to_string());
        }
    }

    let mut body_stmts = codegen_block(&mut cx, &func.body);

    // Strip trailing void return (`return;` or `return undefined`).
    if let Some(ast::Statement::ReturnStatement(ret)) = body_stmts.last() {
        let is_void = ret.argument.is_none()
            || matches!(
                &ret.argument,
                Some(ast::Expression::Identifier(id)) if id.name == "undefined"
            );
        if is_void {
            body_stmts.pop();
        }
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
    // ── Statement-level variants (self-contained, use InstructionValue's lvalue) ──
    match &instr.value {
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            return codegen_declare(cx, lvalue);
        }
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            return codegen_store(cx, lvalue, value);
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            return codegen_destructure(cx, lvalue, value);
        }
        InstructionValue::Debugger { .. } => {
            return Some(cx.builder.statement_debugger(SPAN));
        }
        InstructionValue::StartMemoize { .. } | InstructionValue::FinishMemoize { .. } => {
            return None;
        }
        _ => {}
    }

    // ── Expression-level variants ──
    let expr = codegen_instruction_value(cx, &instr.value)?;

    // No lvalue → expression statement.
    let Some(lvalue) = &instr.lvalue else {
        return Some(cx.builder.statement_expression(SPAN, expr));
    };

    let id = lvalue.identifier.id;

    // Temp inlining decision (matches upstream codegenInstruction):
    // - Unnamed temporaries (name is None or Promoted) → inline into temp map
    // - Named identifiers → always emit as declaration/reassignment
    if is_temp_identifier(&lvalue.identifier) {
        cx.temps.insert(id, Some(expr));
        return None;
    }

    let name = identifier_name(&lvalue.identifier);

    // Already declared → reassignment.
    if cx.declared.contains(&id) {
        return Some(emit_assignment_stmt(cx, &name, expr));
    }

    // New declaration (expression-level always uses Const to match upstream).
    cx.declared.insert(id);
    Some(emit_var_decl_stmt(
        cx,
        &name,
        ast::VariableDeclarationKind::Const,
        Some(expr),
    ))
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
        InstructionValue::RegExpLiteral { pattern, flags, .. } => {
            let re_flags = parse_regexp_flags(flags);
            let regex = ast::RegExp {
                pattern: ast::RegExpPattern {
                    text: cx.builder.atom(pattern),
                    pattern: None,
                },
                flags: re_flags,
            };
            Some(cx.builder.expression_reg_exp_literal(SPAN, regex, None))
        }
        InstructionValue::TaggedTemplateExpression {
            tag, raw, cooked, ..
        } => {
            let tag_expr = codegen_place(cx, tag)?;
            let quasi = cx.builder.template_literal(
                SPAN,
                cx.builder.vec1(cx.builder.template_element(
                    SPAN,
                    ast::TemplateElementValue {
                        raw: cx.builder.atom(raw),
                        cooked: cooked.as_ref().map(|v| cx.builder.atom(v)),
                    },
                    true,
                    false,
                )),
                cx.builder.vec(),
            );
            Some(
                cx.builder
                    .expression_tagged_template(SPAN, tag_expr, NONE, quasi),
            )
        }
        InstructionValue::Await { value, .. } => {
            Some(cx.builder.expression_await(SPAN, codegen_place(cx, value)?))
        }
        InstructionValue::GetIterator { collection, .. } => codegen_place(cx, collection),
        InstructionValue::IteratorNext { collection, .. } => codegen_place(cx, collection),
        InstructionValue::NextPropertyOf { value, .. } => codegen_place(cx, value),
        InstructionValue::ObjectMethod { lowered_func, .. } => {
            super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
                cx.builder,
                None,
                lowered_func,
                FunctionExpressionType::FunctionExpression,
            )
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            let mut prefix_exprs: Vec<ast::Expression<'a>> = Vec::new();
            for seq_instr in instructions {
                let expr = match &seq_instr.value {
                    InstructionValue::StoreLocal { value: v, .. }
                    | InstructionValue::StoreContext { value: v, .. } => codegen_place(cx, v),
                    _ => codegen_instruction_value(cx, &seq_instr.value),
                };
                if let Some(expr) = expr {
                    if let Some(lv) = &seq_instr.lvalue {
                        cx.temps.insert(lv.identifier.id, Some(expr));
                    } else {
                        prefix_exprs.push(expr);
                    }
                }
            }
            let final_expr = codegen_instruction_value(cx, value)?;
            if prefix_exprs.is_empty() {
                Some(final_expr)
            } else {
                prefix_exprs.push(final_expr);
                Some(
                    cx.builder
                        .expression_sequence(SPAN, cx.builder.vec_from_iter(prefix_exprs)),
                )
            }
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            codegen_instruction_value(cx, value)
        }
        InstructionValue::ReactiveLogicalExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_logical(
            SPAN,
            codegen_instruction_value(cx, left)?,
            lower_logical_operator(*operator),
            codegen_instruction_value(cx, right)?,
        )),
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => Some(cx.builder.expression_conditional(
            SPAN,
            codegen_instruction_value(cx, test)?,
            codegen_instruction_value(cx, consequent)?,
            codegen_instruction_value(cx, alternate)?,
        )),
        // Statement-level variants handled in codegen_instruction before reaching here.
        InstructionValue::Debugger { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::Destructure { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Statement-level instruction helpers
// ---------------------------------------------------------------------------

fn codegen_declare<'a>(cx: &mut CodegenContext<'a>, lvalue: &LValue) -> Option<ast::Statement<'a>> {
    let name = lvalue.place.identifier.name.as_ref()?.value().to_string();
    let id = lvalue.place.identifier.id;
    if cx.declared.contains(&id) {
        return None;
    }
    cx.declared.insert(id);
    let kind = variable_declaration_kind(lvalue.kind).unwrap_or(ast::VariableDeclarationKind::Let);
    Some(emit_var_decl_stmt(cx, &name, kind, None))
}

fn codegen_store<'a>(
    cx: &mut CodegenContext<'a>,
    lvalue: &LValue,
    value: &Place,
) -> Option<ast::Statement<'a>> {
    let expr = codegen_place(cx, value)?;
    let id = lvalue.place.identifier.id;

    // Temp inlining: unnamed temporaries (Promoted by rename_variables) are
    // stored for inline substitution, matching upstream behavior.
    // Exception: Reassign kind always emits (it's modifying an existing variable).
    if is_temp_identifier(&lvalue.place.identifier)
        && !cx.declared.contains(&id)
        && !matches!(lvalue.kind, InstructionKind::Reassign)
    {
        cx.temps.insert(id, Some(expr));
        return None;
    }

    let name = identifier_name(&lvalue.place.identifier);

    match lvalue.kind {
        InstructionKind::Reassign => Some(emit_assignment_stmt(cx, &name, expr)),
        InstructionKind::Function | InstructionKind::HoistedFunction
            if matches!(&expr, ast::Expression::FunctionExpression(_)) =>
        {
            if let ast::Expression::FunctionExpression(func_alloc) = expr {
                let mut func = func_alloc.unbox();
                func.id = Some(cx.builder.binding_identifier(SPAN, cx.builder.ident(&name)));
                func.r#type = ast::FunctionType::FunctionDeclaration;
                cx.declared.insert(id);
                Some(ast::Statement::FunctionDeclaration(cx.builder.alloc(func)))
            } else {
                unreachable!()
            }
        }
        kind => {
            if cx.declared.contains(&id) {
                Some(emit_assignment_stmt(cx, &name, expr))
            } else {
                cx.declared.insert(id);
                let decl_kind = match kind {
                    InstructionKind::Const | InstructionKind::HoistedConst => {
                        ast::VariableDeclarationKind::Const
                    }
                    InstructionKind::Function | InstructionKind::HoistedFunction => {
                        ast::VariableDeclarationKind::Const
                    }
                    _ => ast::VariableDeclarationKind::Let,
                };
                Some(emit_var_decl_stmt(cx, &name, decl_kind, Some(expr)))
            }
        }
    }
}

fn codegen_destructure<'a>(
    cx: &mut CodegenContext<'a>,
    lvalue: &LValuePattern,
    value: &Place,
) -> Option<ast::Statement<'a>> {
    let rhs = codegen_place(cx, value)?;
    let mut kind = variable_declaration_kind(lvalue.kind);
    // If all pattern variables are already declared (by scope prologue),
    // treat as reassignment even if the original kind was Let/Const.
    if kind.is_some() && all_pattern_vars_declared(cx, &lvalue.pattern) {
        kind = None;
    }
    if let Some(kind) = kind {
        // Declaration: const [a, b] = expr;
        let pattern = build_binding_pattern_from_pattern(cx, &lvalue.pattern)?;
        Some(ast::Statement::VariableDeclaration(
            cx.builder.alloc_variable_declaration(
                SPAN,
                kind,
                cx.builder.vec1(cx.builder.variable_declarator(
                    SPAN,
                    kind,
                    pattern,
                    NONE,
                    Some(rhs),
                    false,
                )),
                false,
            ),
        ))
    } else {
        // Reassignment: [a, b] = expr;
        let target = build_assignment_target_from_pattern(cx, &lvalue.pattern)?;
        Some(
            cx.builder.statement_expression(
                SPAN,
                cx.builder
                    .expression_assignment(SPAN, AssignmentOperator::Assign, target, rhs),
            ),
        )
    }
}

fn build_binding_pattern_from_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    pattern: &Pattern,
) -> Option<ast::BindingPattern<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = cx.builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        elements.push(Some(
                            cx.builder
                                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&name)),
                        ));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        rest =
                            Some(cx.builder.alloc_binding_rest_element(
                                SPAN,
                                cx.builder.binding_pattern_binding_identifier(
                                    SPAN,
                                    cx.builder.ident(&name),
                                ),
                            ));
                    }
                    ArrayElement::Hole => {
                        if rest.is_some() {
                            return None;
                        }
                        elements.push(None);
                    }
                }
            }
            Some(
                cx.builder
                    .binding_pattern_array_pattern(SPAN, elements, rest),
            )
        }
        Pattern::Object(obj) => {
            let mut properties = cx.builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if rest.is_some() {
                            return None;
                        }
                        let target_name = identifier_name(&property.place.identifier);
                        cx.declared.insert(property.place.identifier.id);
                        let (key, computed) = build_property_key_for_pattern(cx, &property.key)?;
                        let shorthand = matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        );
                        properties.push(cx.builder.binding_property(
                            SPAN,
                            key,
                            cx.builder.binding_pattern_binding_identifier(
                                SPAN,
                                cx.builder.ident(&target_name),
                            ),
                            shorthand,
                            computed,
                        ));
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if rest.is_some() || index + 1 != obj.properties.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        rest =
                            Some(cx.builder.alloc_binding_rest_element(
                                SPAN,
                                cx.builder.binding_pattern_binding_identifier(
                                    SPAN,
                                    cx.builder.ident(&name),
                                ),
                            ));
                    }
                }
            }
            Some(
                cx.builder
                    .binding_pattern_object_pattern(SPAN, properties, rest),
            )
        }
    }
}

fn build_assignment_target_from_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    pattern: &Pattern,
) -> Option<ast::AssignmentTarget<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = cx.builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        elements.push(Some(ast::AssignmentTargetMaybeDefault::from(
                            ast::AssignmentTarget::from(
                                cx.builder
                                    .simple_assignment_target_assignment_target_identifier(
                                        SPAN,
                                        cx.builder.ident(&name),
                                    ),
                            ),
                        )));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        rest = Some(
                            cx.builder.alloc_assignment_target_rest(
                                SPAN,
                                ast::AssignmentTarget::from(
                                    cx.builder
                                        .simple_assignment_target_assignment_target_identifier(
                                            SPAN,
                                            cx.builder.ident(&name),
                                        ),
                                ),
                            ),
                        );
                    }
                    ArrayElement::Hole => {
                        if rest.is_some() {
                            return None;
                        }
                        elements.push(None);
                    }
                }
            }
            Some(ast::AssignmentTarget::from(
                cx.builder
                    .assignment_target_pattern_array_assignment_target(SPAN, elements, rest),
            ))
        }
        Pattern::Object(obj) => {
            let mut properties = cx.builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if rest.is_some() {
                            return None;
                        }
                        let target_name = identifier_name(&property.place.identifier);
                        if matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        ) {
                            properties.push(
                                cx.builder
                                    .assignment_target_property_assignment_target_property_identifier(
                                        SPAN,
                                        cx.builder.identifier_reference(
                                            SPAN,
                                            cx.builder.ident(&target_name),
                                        ),
                                        None,
                                    ),
                            );
                            continue;
                        }
                        let (key, computed) = build_property_key_for_pattern(cx, &property.key)?;
                        properties.push(
                            cx.builder
                                .assignment_target_property_assignment_target_property_property(
                                SPAN,
                                key,
                                ast::AssignmentTargetMaybeDefault::from(
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&target_name),
                                            ),
                                    ),
                                ),
                                computed,
                            ),
                        );
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if rest.is_some() || index + 1 != obj.properties.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        rest = Some(
                            cx.builder.alloc_assignment_target_rest(
                                SPAN,
                                ast::AssignmentTarget::from(
                                    cx.builder
                                        .simple_assignment_target_assignment_target_identifier(
                                            SPAN,
                                            cx.builder.ident(&name),
                                        ),
                                ),
                            ),
                        );
                    }
                }
            }
            Some(ast::AssignmentTarget::from(
                cx.builder
                    .assignment_target_pattern_object_assignment_target(SPAN, properties, rest),
            ))
        }
    }
}

fn build_property_key_for_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    key: &ObjectPropertyKey,
) -> Option<(ast::PropertyKey<'a>, bool)> {
    match key {
        ObjectPropertyKey::Identifier(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) => Some((
            ast::PropertyKey::from(cx.builder.expression_string_literal(
                SPAN,
                cx.builder.atom(name),
                None,
            )),
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
        )),
        ObjectPropertyKey::Computed(place) => {
            Some((ast::PropertyKey::from(codegen_place(cx, place)?), true))
        }
    }
}

// ---------------------------------------------------------------------------
// Emit helpers
// ---------------------------------------------------------------------------

/// Check if an identifier is a temporary (unnamed or promoted by rename_variables).
/// Matches upstream's `identifier.name === null` check.
fn is_temp_identifier(identifier: &Identifier) -> bool {
    match &identifier.name {
        None => true,
        Some(IdentifierName::Promoted(_)) => true,
        Some(IdentifierName::Named(_)) => false,
    }
}

fn identifier_name(identifier: &Identifier) -> String {
    identifier
        .name
        .as_ref()
        .map(|n| n.value().to_string())
        .unwrap_or_else(|| format!("t{}", identifier.id.0))
}

fn variable_declaration_kind(kind: InstructionKind) -> Option<ast::VariableDeclarationKind> {
    match kind {
        InstructionKind::Const | InstructionKind::HoistedConst => {
            Some(ast::VariableDeclarationKind::Const)
        }
        InstructionKind::Let | InstructionKind::Catch | InstructionKind::HoistedLet => {
            Some(ast::VariableDeclarationKind::Let)
        }
        InstructionKind::Reassign
        | InstructionKind::Function
        | InstructionKind::HoistedFunction => None,
    }
}

fn emit_assignment_stmt<'a>(
    cx: &mut CodegenContext<'a>,
    name: &str,
    expr: ast::Expression<'a>,
) -> ast::Statement<'a> {
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
            expr,
        ),
    )
}

fn emit_var_decl_stmt<'a>(
    cx: &mut CodegenContext<'a>,
    name: &str,
    kind: ast::VariableDeclarationKind,
    init: Option<ast::Expression<'a>>,
) -> ast::Statement<'a> {
    // Prevent duplicate `let`/`const` for the same name (can happen when
    // scope declarations and StoreLocal target the same variable via
    // different IdentifierIds after renaming).
    if cx.declared_names.contains(name) {
        if let Some(expr) = init {
            return emit_assignment_stmt(cx, name, expr);
        }
        // Bare duplicate — emit as empty statement that codegen will elide.
        return cx.builder.statement_empty(SPAN);
    }
    cx.declared_names.insert(name.to_string());
    let pattern = cx
        .builder
        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name));
    ast::Statement::VariableDeclaration(
        cx.builder.alloc_variable_declaration(
            SPAN,
            kind,
            cx.builder.vec1(
                cx.builder
                    .variable_declarator(SPAN, kind, pattern, NONE, init, false),
            ),
            false,
        ),
    )
}

fn parse_regexp_flags(flags: &str) -> ast::RegExpFlags {
    let mut result = ast::RegExpFlags::empty();
    for c in flags.chars() {
        result |= match c {
            'g' => ast::RegExpFlags::G,
            'i' => ast::RegExpFlags::I,
            'm' => ast::RegExpFlags::M,
            's' => ast::RegExpFlags::S,
            'u' => ast::RegExpFlags::U,
            'y' => ast::RegExpFlags::Y,
            'd' => ast::RegExpFlags::D,
            'v' => ast::RegExpFlags::V,
            _ => ast::RegExpFlags::empty(),
        };
    }
    result
}

// ---------------------------------------------------------------------------
// Place → Expression
// ---------------------------------------------------------------------------

fn codegen_place<'a>(cx: &mut CodegenContext<'a>, place: &Place) -> Option<ast::Expression<'a>> {
    let id = place.identifier.id;

    // Check temp map first — inlined expression. Use clone_in for
    // multi-use temps (second+ reference gets a deep copy).
    if let Some(temp_slot) = cx.temps.get_mut(&id)
        && let Some(expr) = temp_slot.as_ref()
    {
        return Some(expr.clone_in(cx.allocator));
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
            let alternate_result = alternate.as_ref().map(|alt| codegen_block(cx, alt));
            // Skip empty if/else blocks — emit test as expression statement.
            if consequent_stmts.is_empty()
                && alternate_result.as_ref().is_some_and(|a| a.is_empty())
            {
                return vec![cx.builder.statement_expression(SPAN, test_expr)];
            }
            let consequent_block = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(consequent_stmts));
            let alternate_stmt = alternate_result.and_then(|alt_stmts| {
                if alt_stmts.is_empty() {
                    None // Skip empty else clauses
                } else {
                    Some(
                        cx.builder
                            .statement_block(SPAN, cx.builder.vec_from_iter(alt_stmts)),
                    )
                }
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
                // Wrap case body in a block for Babel-compatible output.
                let wrapped = if consequent.is_empty() {
                    cx.builder.vec()
                } else {
                    cx.builder.vec1(
                        cx.builder
                            .statement_block(SPAN, cx.builder.vec_from_iter(consequent)),
                    )
                };
                switch_cases.push(cx.builder.switch_case(SPAN, test_expr, wrapped));
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
            init, loop_block, ..
        } => {
            // Extract collection from IteratorNext/NextPropertyOf in init,
            // and resolve it BEFORE processing the init block (which may
            // consume the collection's temp).
            let collection_place = extract_for_collection(init);
            let right = if let Some(cp) = &collection_place {
                codegen_place(cx, cp)
            } else {
                None
            };
            let init_stmts = codegen_block(cx, init);
            let left = extract_for_of_left(cx, init_stmts);
            let (Some(left), Some(right)) = (left, right) else {
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
            let collection_place = extract_for_collection(init);
            let right = if let Some(cp) = &collection_place {
                codegen_place(cx, cp)
            } else {
                None
            };
            let init_stmts = codegen_block(cx, init);
            let left = extract_for_of_left(cx, init_stmts);
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
    // Sort by name for deterministic output matching upstream.
    let mut decl_names: Vec<(String, IdentifierId)> = scope
        .declarations
        .iter()
        .filter_map(|(id, decl)| {
            let name = decl.identifier.name.as_ref()?.value().to_string();
            Some((name, *id))
        })
        .collect();
    decl_names.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, id) in &decl_names {
        if !cx.declared.contains(id) {
            cx.declared.insert(*id);
            cx.declared_names.insert(name.clone());
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
                let name_str = name.value().to_string();
                cx.declared_names.insert(name_str.clone());
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
    // Sort dependencies by their rendered name to match upstream ordering.
    let mut sorted_deps: Vec<&ReactiveScopeDependency> = scope.dependencies.iter().collect();
    sorted_deps.sort_by_key(|d| dep_sort_key(d));
    let deps: Vec<(u32, ast::Expression<'a>)> = sorted_deps
        .iter()
        .filter_map(|dep| {
            let dep_expr = codegen_dependency_expr(cx, dep)?;
            let slot = cx.alloc_cache_slot();
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
            Some((name, cx.alloc_cache_slot()))
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
                cx.builder.statement_block(
                    SPAN,
                    cx.builder.vec1(
                        cx.builder
                            .statement_return(SPAN, Some(cx.ident_expr(&name))),
                    ),
                ),
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
    let prop_expr = codegen_place(cx, property)?;

    // If the property already resolved to a member expression (e.g., x.push
    // from a PropertyLoad temp), use it directly as the callee — the receiver
    // binding is already embedded in the member expression.
    if matches!(
        &prop_expr,
        ast::Expression::StaticMemberExpression(_) | ast::Expression::ComputedMemberExpression(_)
    ) {
        return Some(prop_expr);
    }

    // If the property is a string literal that is a valid identifier,
    // build a static member expression (x.push instead of x["push"]).
    if let ast::Expression::StringLiteral(lit) = &prop_expr
        && is_identifier_name(&lit.value)
    {
        return Some(ast::Expression::from(
            cx.builder.member_expression_static(
                SPAN,
                codegen_place(cx, receiver)?,
                cx.builder
                    .identifier_name(SPAN, cx.builder.ident(lit.value.as_str())),
                receiver_optional,
            ),
        ));
    }

    // Default: computed member expression.
    Some(ast::Expression::from(
        cx.builder.member_expression_computed(
            SPAN,
            codegen_place(cx, receiver)?,
            prop_expr,
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

fn all_pattern_vars_declared(cx: &CodegenContext<'_>, pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Array(arr) => arr.items.iter().all(|item| match item {
            ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                let name = identifier_name(&p.identifier);
                cx.declared_names.contains(&name)
            }
            ArrayElement::Hole => true,
        }),
        Pattern::Object(obj) => obj.properties.iter().all(|prop| match prop {
            ObjectPropertyOrSpread::Property(p) => {
                let name = identifier_name(&p.place.identifier);
                cx.declared_names.contains(&name)
            }
            ObjectPropertyOrSpread::Spread(p) => {
                let name = identifier_name(&p.identifier);
                cx.declared_names.contains(&name)
            }
        }),
    }
}

/// Try to inline a zero-dependency (sentinel) scope by storing its output
/// expression in the temp map. Returns true if the scope was inlined (should
/// be skipped in output), false if it should be emitted normally.
/// Build a sort key for a scope dependency (for deterministic ordering).
fn dep_sort_key(dep: &ReactiveScopeDependency) -> String {
    let root = dep
        .identifier
        .name
        .as_ref()
        .map(|n| n.value().to_string())
        .unwrap_or_else(|| format!("t{}", dep.identifier.id.0));
    if dep.path.is_empty() {
        return root;
    }
    let mut key = root;
    for entry in &dep.path {
        key.push('.');
        key.push_str(&entry.property);
    }
    key
}

/// Extract the collection Place from a for-of/for-in init block by scanning
/// for `IteratorNext` or `NextPropertyOf` instructions.
fn extract_for_collection(init: &ReactiveBlock) -> Option<Place> {
    for stmt in init {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        match &instr.value {
            InstructionValue::IteratorNext { collection, .. } => {
                return Some(collection.clone());
            }
            InstructionValue::NextPropertyOf { value, .. } => {
                return Some(value.clone());
            }
            _ => {}
        }
    }
    None
}

/// Extract the for-of/for-in left-hand side from processed init statements.
fn extract_for_of_left<'a>(
    _cx: &mut CodegenContext<'a>,
    init_stmts: Vec<ast::Statement<'a>>,
) -> Option<ast::ForStatementLeft<'a>> {
    init_stmts.into_iter().find_map(|s| {
        if let ast::Statement::VariableDeclaration(mut decl) = s {
            // Strip initializer — for-of/for-in left side is pattern only.
            for declarator in decl.declarations.iter_mut() {
                declarator.init = None;
            }
            Some(ast::ForStatementLeft::VariableDeclaration(decl))
        } else {
            None
        }
    })
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
