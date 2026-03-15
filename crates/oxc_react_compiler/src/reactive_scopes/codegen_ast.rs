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

/// Sentinel value for uninitialized cache slots.
pub const MEMO_CACHE_SENTINEL: &str = "react.memo_cache_sentinel";
/// Sentinel value for early return detection.
pub const EARLY_RETURN_SENTINEL: &str = "react.early_return_sentinel";

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

pub struct CodegenOptions {
    pub enable_change_variable_codegen: bool,
    pub enable_emit_hook_guards: bool,
    pub enable_change_detection_for_debugging: bool,
    pub enable_reset_cache_on_source_file_changes: bool,
    pub fast_refresh_source_hash: Option<String>,
    pub disable_memoization_features: bool,
    pub disable_memoization_for_debugging: bool,
    pub fbt_operands: HashSet<IdentifierId>,
    /// Cache binding name override (e.g., "$0" when "$" is renamed to avoid conflicts).
    pub cache_binding_name: Option<String>,
    /// Set of all used identifier names (from rename_variables). Used to resolve
    /// temp naming collisions when the same Promoted name appears in different scopes.
    pub unique_identifiers: HashSet<String>,
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
    /// Identifiers that have been declared (to avoid re-declaring), tracked by IdentifierId.
    declared: HashSet<IdentifierId>,
    /// Declaration IDs that have been declared (to avoid duplicates from different SSA ids).
    declared_decl_ids: HashSet<DeclarationId>,
    /// Variable names that have been declared (to avoid duplicate `let` for same name).
    declared_names: HashSet<String>,
    /// Temp expressions: keyed by DeclarationId (shared across SSA uses) to match
    /// upstream's `cx.temp.set(declarationId, value)` / `cx.temp.get(declarationId)`.
    temps: HashMap<DeclarationId, Option<ast::Expression<'a>>>,
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
    /// Function name for structural check diagnostics.
    #[allow(dead_code)]
    fn_name: String,
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
    let cache_binding = options
        .cache_binding_name
        .clone()
        .unwrap_or_else(|| "$".to_string());

    let mut cx = CodegenContext {
        builder,
        allocator,
        declared: HashSet::new(),
        declared_decl_ids: HashSet::new(),
        declared_names: HashSet::new(),
        temps: HashMap::new(),
        next_cache_index: 0,
        cache_binding: cache_binding.clone(),
        emitted_hook_guards: false,
        needs_function_hook_guard_wrapper: false,
        needs_structural_check: false,
        options,
        fn_name: func.id.clone().unwrap_or_default(),
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

    // Allocate fast-refresh slot BEFORE body codegen so it gets slot 0
    // (matching upstream behavior in CodegenReactiveFunction.ts).
    let fast_refresh_slot = if cx.options.enable_reset_cache_on_source_file_changes
        && cx.options.fast_refresh_source_hash.is_some()
    {
        Some(cx.alloc_cache_slot())
    } else {
        None
    };

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
        let fast_refresh = fast_refresh_slot.and_then(|slot| {
            cx.options
                .fast_refresh_source_hash
                .as_ref()
                .map(|hash| FastRefreshPrologue {
                    cache_index: slot,
                    hash: hash.clone(),
                    index_binding_name: format!("${}", cache_binding),
                })
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
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            // Upstream: for StoreLocal/Reassign with a temp instruction-level
            // lvalue, inline the assignment expression into the temp map.
            if matches!(lvalue.kind, InstructionKind::Reassign)
                && let Some(outer_lv) = &instr.lvalue
                && is_temp_identifier(&outer_lv.identifier)
            {
                let rhs = codegen_place(cx, value)?;
                let assign_name = identifier_name(&lvalue.place.identifier);
                let assign_expr = cx.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        cx.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                cx.builder.ident(&assign_name),
                            ),
                    ),
                    rhs,
                );
                cx.temps
                    .insert(outer_lv.identifier.declaration_id, Some(assign_expr));
                return None;
            }
            return codegen_store(cx, lvalue, value);
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            // Same chained-assignment logic as StoreLocal above.
            if matches!(lvalue.kind, InstructionKind::Reassign)
                && let Some(outer_lv) = &instr.lvalue
                && is_temp_identifier(&outer_lv.identifier)
            {
                let rhs = codegen_place(cx, value)?;
                let assign_name = identifier_name(&lvalue.place.identifier);
                let assign_expr = cx.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        cx.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                cx.builder.ident(&assign_name),
                            ),
                    ),
                    rhs,
                );
                cx.temps
                    .insert(outer_lv.identifier.declaration_id, Some(assign_expr));
                return None;
            }
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

    let decl_id = lvalue.identifier.declaration_id;

    // Temp inlining decision (matches upstream codegenInstruction):
    // - Unnamed temporaries (name is None or Promoted) → inline into temp map
    // - Named identifiers → always emit as declaration/reassignment
    if is_temp_identifier(&lvalue.identifier) {
        cx.temps.insert(decl_id, Some(expr));
        return None;
    }

    let name = identifier_name(&lvalue.identifier);
    let id = lvalue.identifier.id;

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
        } => {
            let callee_expr = codegen_place(cx, callee)?;
            let is_hook = cx.options.enable_emit_hook_guards
                && !cx.options.disable_memoization_features
                && expr_is_hook_name(&callee_expr);
            let call_expr = cx.builder.expression_call(
                SPAN,
                callee_expr,
                NONE,
                codegen_arguments(cx, args)?,
                *optional,
            );
            if is_hook {
                Some(wrap_hook_guard_iife(cx, call_expr))
            } else {
                Some(call_expr)
            }
        }
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
            let callee_expr =
                codegen_method_call_callee(cx, receiver, property, *receiver_optional)?;
            // For method calls, the hook name is the property (e.g., obj.useIdentity).
            let is_hook = cx.options.enable_emit_hook_guards
                && !cx.options.disable_memoization_features
                && method_property_is_hook_name(&callee_expr);
            let call_expr = cx.builder.expression_call(
                SPAN,
                callee_expr,
                NONE,
                codegen_arguments(cx, args)?,
                *call_optional,
            );
            if is_hook {
                Some(wrap_hook_guard_iife(cx, call_expr))
            } else {
                Some(call_expr)
            }
        }
        InstructionValue::TypeCastExpression {
            value,
            type_annotation,
            type_annotation_kind,
            ..
        } => {
            let inner = codegen_place(cx, value)?;
            // Preserve TypeScript `as T` annotations.
            if matches!(type_annotation_kind, TypeAnnotationKind::As) {
                Some(cx.builder.expression_ts_as(
                    SPAN,
                    inner,
                    cx.builder.ts_type_type_reference(
                        SPAN,
                        cx.builder.ts_type_name_identifier_reference(
                            SPAN,
                            cx.builder.ident(type_annotation),
                        ),
                        NONE,
                    ),
                ))
            } else {
                Some(inner)
            }
        }
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
        } => {
            let result = super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
                cx.builder,
                name.as_deref(),
                lowered_func,
                *expr_type,
            );
            if result.is_some() {
                result
            } else {
                lower_function_expression_via_reactive(
                    cx,
                    name.as_deref(),
                    lowered_func,
                    *expr_type,
                )
            }
        }
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
            let result = super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
                cx.builder,
                None,
                lowered_func,
                FunctionExpressionType::FunctionExpression,
            );
            if result.is_some() {
                result
            } else {
                lower_function_expression_via_reactive(
                    cx,
                    None,
                    lowered_func,
                    FunctionExpressionType::FunctionExpression,
                )
            }
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            let mut prefix_exprs: Vec<ast::Expression<'a>> = Vec::new();
            for seq_instr in instructions {
                let expr = match &seq_instr.value {
                    InstructionValue::StoreLocal {
                        lvalue: store_lv,
                        value: v,
                        ..
                    }
                    | InstructionValue::StoreContext {
                        lvalue: store_lv,
                        value: v,
                        ..
                    } => {
                        let rhs = codegen_place(cx, v);
                        // Build assignment expression for reassign stores.
                        if matches!(store_lv.kind, InstructionKind::Reassign) {
                            rhs.map(|rhs_expr| {
                                let name = identifier_name(&store_lv.place.identifier);
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
                                    rhs_expr,
                                )
                            })
                        } else {
                            rhs
                        }
                    }
                    _ => codegen_instruction_value(cx, &seq_instr.value),
                };
                if let Some(expr) = expr {
                    if let Some(lv) = &seq_instr.lvalue {
                        if is_temp_identifier(&lv.identifier) {
                            cx.temps.insert(
                                lv.identifier.declaration_id,
                                Some(expr.clone_in(cx.allocator)),
                            );
                        }
                        // Always add to prefix for side effects.
                        prefix_exprs.push(expr);
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
    let decl_id = lvalue.place.identifier.declaration_id;
    if cx.declared.contains(&id) || cx.declared_decl_ids.contains(&decl_id) {
        return None;
    }
    cx.declared.insert(id);
    cx.declared_decl_ids.insert(decl_id);
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
        cx.temps
            .insert(lvalue.place.identifier.declaration_id, Some(expr));
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

/// Check if an identifier is a temporary (truly unnamed).
/// Promoted `tN` names are runtime bindings that earlier passes chose to
/// materialize, so re-inlining them breaks evaluation order. Only truly
/// unnamed identifiers are eligible for temp inlining (matches string
/// codegen's `is_temp_like_identifier`).
fn is_temp_identifier(identifier: &Identifier) -> bool {
    identifier.name.is_none()
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
    let decl_id = place.identifier.declaration_id;

    // Only check temp map for unnamed/promoted identifiers (temporaries).
    // Named identifiers always emit as identifier references, even if
    // something was stored in the temp map for their declaration_id.
    if is_temp_identifier(&place.identifier)
        && let Some(temp_slot) = cx.temps.get_mut(&decl_id)
        && let Some(expr) = temp_slot.as_ref()
    {
        return Some(expr.clone_in(cx.allocator));
    }

    // Use identifier name.
    if let Some(name) = place.identifier.name.as_ref() {
        return Some(cx.ident_expr(name.value()));
    }

    // Fallback: use temp name.
    Some(cx.ident_expr(&format!("t{}", decl_id.0)))
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
                let mut consequent = case
                    .block
                    .as_ref()
                    .map(|b| codegen_block(cx, b))
                    .unwrap_or_default();
                // If the case body is only a `break;` with no other content,
                // treat it as fallthrough (suppress the break).
                // If the case body is only a bare `break;`, treat as fallthrough.
                if consequent.len() == 1
                    && matches!(&consequent[0], ast::Statement::BreakStatement(b) if b.label.is_none())
                {
                    consequent.clear();
                }
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
            // Init: emit instructions from init block, reconstructing merged
            // declarations.  The HIR may split `let i = expr;` into separate
            // DeclareLocal + StoreLocal instructions, producing `let i;` then
            // `i = expr;`.  We merge these back into a single VariableDeclaration
            // with the initializer, matching upstream Babel output.
            let init_stmts = codegen_block(cx, init);
            let for_init = reconstruct_for_init(cx, init_stmts);

            let test_expr = codegen_place(cx, test);

            // Update expression.
            //
            // Strategy: process the update block's instructions to populate
            // the temp map, then find the StoreLocal/Reassign instruction
            // within the block to construct the assignment expression.  The
            // `update_value` may be a trailing `LoadLocal(x)` that merely
            // reads back the result; the actual side-effect we need is the
            // `StoreLocal { x = expr }` inside the block.
            let update_expr = if let Some(update_block) = update {
                // Process all instructions into the temp map / statement list.
                let update_stmts = codegen_block(cx, update_block);

                // Prefer: find the assignment expression produced by the
                // StoreLocal/Reassign in the emitted statements.
                let assign_expr = update_stmts.iter().rev().find_map(|s| {
                    if let ast::Statement::ExpressionStatement(es) = s {
                        if matches!(&es.expression, ast::Expression::AssignmentExpression(_)) {
                            Some(es.expression.clone_in(cx.allocator))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                if assign_expr.is_some() {
                    assign_expr
                } else if let Some(uv) = update_value {
                    // Fallback to codegen_instruction_value on the
                    // update_value when no assignment was found.
                    match uv.as_ref() {
                        InstructionValue::StoreLocal { lvalue, value, .. }
                        | InstructionValue::StoreContext { lvalue, value, .. }
                            if matches!(lvalue.kind, InstructionKind::Reassign) =>
                        {
                            if let Some(rhs) = codegen_place(cx, value) {
                                let name = identifier_name(&lvalue.place.identifier);
                                Some(cx.builder.expression_assignment(
                                    SPAN,
                                    AssignmentOperator::Assign,
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&name),
                                            ),
                                    ),
                                    rhs,
                                ))
                            } else {
                                None
                            }
                        }
                        _ => codegen_instruction_value(cx, uv),
                    }
                } else {
                    // Extract expression from last statement.
                    update_stmts.into_iter().last().and_then(|s| {
                        if let ast::Statement::ExpressionStatement(es) = s {
                            Some(es.unbox().expression)
                        } else {
                            None
                        }
                    })
                }
            } else if let Some(uv) = update_value {
                codegen_instruction_value(cx, uv)
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
    let mut decl_names: Vec<(String, IdentifierId, DeclarationId)> = scope
        .declarations
        .iter()
        .filter_map(|(id, decl)| {
            let name = decl.identifier.name.as_ref()?.value().to_string();
            Some((name, *id, decl.identifier.declaration_id))
        })
        .collect();
    decl_names.sort_by(|a, b| a.0.cmp(&b.0));

    // Collect scope variable declarations.
    // When enable_change_variable_codegen is active with deps, these are deferred
    // to emit after the change variable declarations.
    let mut scope_decl_stmts: Vec<ast::Statement<'a>> = Vec::new();
    for (name, id, decl_id) in &decl_names {
        if !cx.declared.contains(id) && !cx.declared_decl_ids.contains(decl_id) {
            cx.declared.insert(*id);
            cx.declared_decl_ids.insert(*decl_id);
            cx.declared_names.insert(name.clone());
            let pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name));
            scope_decl_stmts.push(ast::Statement::VariableDeclaration(
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
        let r_decl_id = reassign.declaration_id;
        if !cx.declared.contains(&id) && !cx.declared_decl_ids.contains(&r_decl_id) {
            if let Some(name) = reassign.name.as_ref() {
                let name_str = name.value().to_string();
                if cx.declared_names.contains(&name_str) {
                    // Already declared by name (e.g., from DeclareLocal). Track ids only.
                    cx.declared.insert(id);
                    cx.declared_decl_ids.insert(r_decl_id);
                } else {
                    cx.declared.insert(id);
                    cx.declared_decl_ids.insert(r_decl_id);
                    cx.declared_names.insert(name_str);
                    let pattern = cx
                        .builder
                        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name.value()));
                    scope_decl_stmts.push(ast::Statement::VariableDeclaration(
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
            } else {
                cx.declared.insert(id);
                cx.declared_decl_ids.insert(r_decl_id);
            }
        }
    }

    // Emit scope declarations now (will be deferred for change variable codegen).
    let defer_decls = cx.options.enable_change_variable_codegen && !scope.dependencies.is_empty();
    if !defer_decls {
        stmts.append(&mut scope_decl_stmts);
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
        .map(|(name, _, _)| {
            let slot = cx.alloc_cache_slot();
            (name.clone(), slot)
        })
        .collect();

    // Deduplicate reassignments against declarations to avoid storing
    // the same variable in two separate cache slots.
    let output_decl_ids: HashSet<DeclarationId> = decl_names.iter().map(|(_, _, d)| *d).collect();
    let reassign_slots: Vec<(String, u32)> = scope
        .reassignments
        .iter()
        .filter_map(|reassign| {
            if output_decl_ids.contains(&reassign.declaration_id) {
                return None;
            }
            let name = reassign.name.as_ref()?.value().to_string();
            Some((name, cx.alloc_cache_slot()))
        })
        .collect();

    // Build the scope body.
    let mut body_stmts = codegen_block(cx, instructions);

    // Post-process: fuse test variables and setup statements into ternary
    // expressions to match upstream's ternary restructuring.
    fuse_scope_body_ternaries(cx, &mut body_stmts);

    // --- Change detection for debugging ---
    if cx.options.enable_change_detection_for_debugging && !deps.is_empty() {
        cx.needs_structural_check = true;
        let all_outputs: Vec<(String, u32)> = output_slots
            .iter()
            .chain(reassign_slots.iter())
            .cloned()
            .collect();

        // Build condition: $[0] !== dep0 || $[1] !== dep1 || ...
        let condition_expr = deps
            .iter()
            .map(|(slot, dep_expr)| {
                cx.builder.expression_binary(
                    SPAN,
                    cx.cache_access(*slot),
                    oxc_syntax::operator::BinaryOperator::StrictInequality,
                    dep_expr.clone_in(cx.allocator),
                )
            })
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        // Scope location string for diagnostics.
        let scope_loc = format_change_detection_scope_loc(scope, instructions);

        let mut block_stmts: Vec<ast::Statement<'a>> = Vec::new();

        // 1. Execute body unconditionally.
        block_stmts.extend(body_stmts);

        // 2. let condition = deps_test;
        let condition_pattern = cx
            .builder
            .binding_pattern_binding_identifier(SPAN, cx.builder.ident("condition"));
        block_stmts.push(ast::Statement::VariableDeclaration(
            cx.builder.alloc_variable_declaration(
                SPAN,
                ast::VariableDeclarationKind::Let,
                cx.builder.vec1(cx.builder.variable_declarator(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    condition_pattern,
                    NONE,
                    Some(condition_expr),
                    false,
                )),
                false,
            ),
        ));

        // 3. if (!condition) { let old$name = $[slot]; $structuralCheck(..., "cached", ...) }
        let mut cached_check_stmts: Vec<ast::Statement<'a>> = Vec::new();
        for (name, slot) in &all_outputs {
            let old_name = format!("old${name}");
            let old_pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&old_name));
            cached_check_stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        old_pattern,
                        NONE,
                        Some(cx.cache_access(*slot)),
                        false,
                    )),
                    false,
                ),
            ));
            cached_check_stmts.push(build_structural_check_call(
                cx, &old_name, name, name, "cached", &scope_loc,
            ));
        }

        if !cached_check_stmts.is_empty() {
            block_stmts.push(
                cx.builder.statement_if(
                    SPAN,
                    cx.builder.expression_unary(
                        SPAN,
                        oxc_syntax::operator::UnaryOperator::LogicalNot,
                        cx.ident_expr("condition"),
                    ),
                    cx.builder
                        .statement_block(SPAN, cx.builder.vec_from_iter(cached_check_stmts)),
                    None,
                ),
            );
        }

        // 4. Store deps and outputs.
        for (slot, dep_expr) in deps {
            block_stmts.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
        for (name, slot) in &all_outputs {
            block_stmts.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        // 5. if (condition) { body_again; $structuralCheck(..., "recomputed", ...); name = $[slot] }
        // Re-generate the body for the recomputed path.
        let recompute_body = codegen_block(cx, instructions);
        let mut recompute_stmts: Vec<ast::Statement<'a>> = Vec::new();
        recompute_stmts.extend(recompute_body);
        for (name, slot) in &all_outputs {
            recompute_stmts.push(build_structural_check_call(
                cx,
                &format!("$[{slot}]"),
                name,
                name,
                "recomputed",
                &scope_loc,
            ));
            recompute_stmts.push(
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

        block_stmts.push(
            cx.builder.statement_if(
                SPAN,
                cx.ident_expr("condition"),
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(recompute_stmts)),
                None,
            ),
        );

        // Wrap in a block statement.
        stmts.push(
            cx.builder
                .statement_block(SPAN, cx.builder.vec_from_iter(block_stmts)),
        );
        return stmts;
    }

    if deps.is_empty() {
        // Zero-dependency: use sentinel check.
        let sentinel_slot = if !output_slots.is_empty() {
            output_slots[0].1
        } else if !reassign_slots.is_empty() {
            reassign_slots[0].1
        } else {
            cx.alloc_cache_slot()
        };

        let mut test = cx.builder.expression_binary(
            SPAN,
            cx.cache_access(sentinel_slot),
            oxc_syntax::operator::BinaryOperator::StrictEquality,
            cx.sentinel_expr(),
        );

        // disable_memoization_for_debugging: always recompute (append `|| true`)
        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

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
    } else if cx.options.enable_change_variable_codegen {
        // Change variable codegen: emit explicit comparison variables.
        // `const c_0 = $[0] !== dep0; const c_1 = $[1] !== dep1; if (c_0 || c_1) { ... }`
        let mut change_var_names: Vec<String> = Vec::new();
        for (i, (slot, dep_expr)) in deps.iter().enumerate() {
            let var_name = format!("c_{i}");
            let comparison = cx.builder.expression_binary(
                SPAN,
                cx.cache_access(*slot),
                oxc_syntax::operator::BinaryOperator::StrictInequality,
                dep_expr.clone_in(cx.allocator),
            );
            let pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&var_name));
            stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Const,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Const,
                        pattern,
                        NONE,
                        Some(comparison),
                        false,
                    )),
                    false,
                ),
            ));
            change_var_names.push(var_name);
        }

        // Emit deferred scope declarations after change variables.
        stmts.extend(scope_decl_stmts);

        let mut test = change_var_names
            .iter()
            .map(|name| cx.ident_expr(name))
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

        // If body: recompute + store deps + store outputs.
        let mut if_body = body_stmts;
        for (slot, dep_expr) in deps {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
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

        let mut test = test_parts
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

        // disable_memoization_for_debugging: always recompute (append `|| true`)
        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

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
        // Use computed member access for numeric property names (e.g., x[0][1])
        // and static member access for identifier-like names (e.g., x.foo).
        let is_numeric = entry.property.parse::<f64>().is_ok();
        if is_numeric {
            let num_val = entry.property.parse::<f64>().unwrap();
            expr = ast::Expression::from(
                cx.builder.member_expression_computed(
                    SPAN,
                    expr,
                    cx.builder
                        .expression_numeric_literal(SPAN, num_val, None, NumberBase::Decimal),
                    entry.optional,
                ),
            );
        } else if entry.optional {
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

/// Mark all terminal labels in a reactive block as implicit, preventing
/// them from being emitted as JavaScript labels. Used for inner function
/// expressions where CFG block IDs should not become visible labels.
fn suppress_labels_recursive(block: &mut ReactiveBlock) {
    for stmt in block.iter_mut() {
        match stmt {
            ReactiveStatement::Terminal(term_stmt) => {
                if let Some(label) = &mut term_stmt.label {
                    label.implicit = true;
                }
                match &mut term_stmt.terminal {
                    ReactiveTerminal::If {
                        consequent,
                        alternate,
                        ..
                    } => {
                        suppress_labels_recursive(consequent);
                        if let Some(alt) = alternate {
                            suppress_labels_recursive(alt);
                        }
                    }
                    ReactiveTerminal::Switch { cases, .. } => {
                        for case in cases {
                            if let Some(block) = &mut case.block {
                                suppress_labels_recursive(block);
                            }
                        }
                    }
                    ReactiveTerminal::For {
                        init, loop_block, ..
                    } => {
                        suppress_labels_recursive(init);
                        suppress_labels_recursive(loop_block);
                    }
                    ReactiveTerminal::While { loop_block, .. }
                    | ReactiveTerminal::DoWhile { loop_block, .. }
                    | ReactiveTerminal::ForOf { loop_block, .. }
                    | ReactiveTerminal::ForIn { loop_block, .. } => {
                        suppress_labels_recursive(loop_block);
                    }
                    ReactiveTerminal::Try {
                        block: try_block,
                        handler,
                        ..
                    } => {
                        suppress_labels_recursive(try_block);
                        suppress_labels_recursive(handler);
                    }
                    _ => {}
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                suppress_labels_recursive(&mut scope_block.instructions);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                suppress_labels_recursive(&mut pruned.instructions);
            }
            _ => {}
        }
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

#[allow(dead_code)]
fn dump_reactive_block(block: &ReactiveBlock, indent: usize) {
    let pad = "  ".repeat(indent);
    for stmt in block.iter() {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                let lv = instr
                    .lvalue
                    .as_ref()
                    .map(|p| identifier_name(&p.identifier))
                    .unwrap_or_default();
                eprintln!(
                    "{pad}Instr({lv}): {:?}",
                    std::mem::discriminant(&instr.value)
                );
            }
            ReactiveStatement::Terminal(term) => {
                eprintln!(
                    "{pad}Terminal: {:?}",
                    std::mem::discriminant(&term.terminal)
                );
            }
            ReactiveStatement::Scope(scope) => {
                let decls: Vec<String> = scope
                    .scope
                    .declarations
                    .values()
                    .map(|d| {
                        d.identifier
                            .name
                            .as_ref()
                            .map(|n| n.value().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                eprintln!("{pad}Scope(decls={decls:?}):");
                dump_reactive_block(&scope.instructions, indent + 1);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                eprintln!("{pad}PrunedScope:");
                dump_reactive_block(&pruned.instructions, indent + 1);
            }
        }
    }
}

/// Reconstruct a for-loop init from the emitted statements.
///
/// The HIR may split `let i = expr` into separate DeclareLocal (`let i;`) and
/// StoreLocal (`i = expr;`) instructions.  This function merges them back:
///
/// - `[let i;, i = expr;]` → `let i = expr`
/// - `[let i = 0;, let len = expr;]` → `let i = 0, len = expr`
/// - `[let i;]` → `let i`
///
/// Returns `None` when the statements cannot be reconstructed into a single
/// `ForStatementInit`.
fn reconstruct_for_init<'a>(
    cx: &mut CodegenContext<'a>,
    stmts: Vec<ast::Statement<'a>>,
) -> Option<ast::ForStatementInit<'a>> {
    if stmts.is_empty() {
        return None;
    }

    // Fast path: single statement.
    if stmts.len() == 1 {
        let stmt = stmts.into_iter().next().unwrap();
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                return Some(ast::ForStatementInit::VariableDeclaration(decl));
            }
            // Single assignment: convert `VAR = EXPR;` to `let VAR = EXPR`
            // for for-init position when the variable was already hoisted by
            // scope declarations.
            ast::Statement::ExpressionStatement(es) => {
                if let ast::Expression::AssignmentExpression(assign) = es.unbox().expression
                    && assign.operator == AssignmentOperator::Assign
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(ref ident) =
                        assign.left
                {
                    let name = ident.name.to_string();
                    let pattern = cx
                        .builder
                        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&name));
                    let kind = ast::VariableDeclarationKind::Let;
                    return Some(ast::ForStatementInit::VariableDeclaration(
                        cx.builder.alloc_variable_declaration(
                            SPAN,
                            kind,
                            cx.builder.vec1(cx.builder.variable_declarator(
                                SPAN,
                                kind,
                                pattern,
                                NONE,
                                Some(assign.unbox().right),
                                false,
                            )),
                            false,
                        ),
                    ));
                }
                return None;
            }
            _ => return None,
        }
    }

    // Multi-statement path: merge DeclareLocal + assignment pairs.
    // Collect all declarators from variable declarations, then merge in
    // assignments for uninitialized ones.
    struct DeclInfo<'a> {
        name: String,
        id: ast::BindingPattern<'a>,
        init: Option<ast::Expression<'a>>,
    }
    let mut declarators: Vec<DeclInfo<'a>> = Vec::new();
    let mut kind = ast::VariableDeclarationKind::Let;

    for stmt in stmts {
        match stmt {
            ast::Statement::VariableDeclaration(mut decl) => {
                kind = decl.kind;
                for d in decl.declarations.drain(..) {
                    let name = match &d.id {
                        ast::BindingPattern::BindingIdentifier(id) => id.name.to_string(),
                        _ => String::new(),
                    };
                    declarators.push(DeclInfo {
                        name,
                        id: d.id,
                        init: d.init,
                    });
                }
            }
            ast::Statement::ExpressionStatement(es) => {
                if let ast::Expression::AssignmentExpression(assign) = es.unbox().expression
                    && assign.operator == AssignmentOperator::Assign
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(ident) = &assign.left
                {
                    let name = ident.name.as_str();
                    // Find the matching uninitialised declarator and set its init.
                    if let Some(d) = declarators
                        .iter_mut()
                        .rev()
                        .find(|d| d.init.is_none() && d.name == name)
                    {
                        d.init = Some(assign.unbox().right);
                        continue;
                    }
                }
                // Could not merge — bail out.
                return None;
            }
            _ => return None,
        }
    }

    if declarators.is_empty() {
        return None;
    }

    let mut oxc_declarators = cx.builder.vec_with_capacity(declarators.len());
    for d in declarators {
        oxc_declarators.push(
            cx.builder
                .variable_declarator(SPAN, kind, d.id, NONE, d.init, false),
        );
    }
    Some(ast::ForStatementInit::VariableDeclaration(
        cx.builder
            .alloc_variable_declaration(SPAN, kind, oxc_declarators, false),
    ))
}

// ---------------------------------------------------------------------------
// Change detection helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Ternary fusion post-processing
// ---------------------------------------------------------------------------

/// Detect and fuse patterns like:
/// ```js
/// let t0 = TEST;
/// SETUP;
/// t0 ? CONSEQUENT : ALT;
/// ```
/// into:
/// ```js
/// TEST ? ((SETUP_EXPR), CONSEQUENT) : ALT;
/// ```
/// This matches the upstream string codegen's ternary restructuring.
fn fuse_scope_body_ternaries<'a>(cx: &mut CodegenContext<'a>, stmts: &mut Vec<ast::Statement<'a>>) {
    // Look for the pattern: const/let TEMP = EXPR; ... TEMP ? A : B;
    // where TEMP is only used as the ternary test.
    let mut i = 0;
    while i < stmts.len() {
        // Find a variable declaration `let tN = EXPR;`
        let test_name = match &stmts[i] {
            ast::Statement::VariableDeclaration(decl)
                if decl.declarations.len() == 1
                    && decl.declarations[0].init.is_some()
                    && matches!(
                        &decl.declarations[0].id,
                        ast::BindingPattern::BindingIdentifier(_)
                    ) =>
            {
                let d = &decl.declarations[0];
                let name = if let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                    id.name.to_string()
                } else {
                    i += 1;
                    continue;
                };
                // Only match temp-like names (t0, t1, etc.)
                if !name.starts_with('t') || !name[1..].chars().all(|c| c.is_ascii_digit()) {
                    i += 1;
                    continue;
                }
                name
            }
            _ => {
                i += 1;
                continue;
            }
        };

        // Look ahead for setup statements (expressions and temp declarations)
        // and a ternary using test_name.
        let mut ternary_idx = None;
        for (j, stmt) in stmts.iter().enumerate().skip(i + 1) {
            if let ast::Statement::ExpressionStatement(es) = stmt
                && let ast::Expression::ConditionalExpression(cond) = &es.expression
                && let ast::Expression::Identifier(id) = &cond.test
                && id.name.as_str() == test_name
            {
                ternary_idx = Some(j);
                break;
            } else if matches!(
                stmt,
                ast::Statement::ExpressionStatement(_) | ast::Statement::VariableDeclaration(_)
            ) {
                continue; // Expression statements and temp declarations are OK
            } else {
                break; // Other statement types break the pattern
            }
        }

        let Some(ternary_pos) = ternary_idx else {
            i += 1;
            continue;
        };

        // Extract the test expression from the declaration.
        let test_decl_stmt = stmts.remove(i);
        let test_init = if let ast::Statement::VariableDeclaration(mut decl) = test_decl_stmt {
            decl.declarations.drain(..).next().and_then(|d| d.init)
        } else {
            None
        };
        let Some(real_test) = test_init else {
            i += 1;
            continue;
        };

        // Collect setup expressions between the declaration and the ternary.
        // Also handle temp variable declarations (`let t1 = EXPR;`) by
        // converting them to assignment expressions for sequence fusion.
        let setup_count = ternary_pos - 1 - i; // positions shifted after remove
        let mut setup_exprs: Vec<ast::Expression<'a>> = Vec::new();
        let mut temp_decls: HashMap<String, ast::Expression<'a>> = HashMap::new();
        for _ in 0..setup_count {
            let setup_stmt = stmts.remove(i);
            match setup_stmt {
                ast::Statement::ExpressionStatement(es) => {
                    setup_exprs.push(es.unbox().expression);
                }
                ast::Statement::VariableDeclaration(mut decl) => {
                    // Convert `let tN = EXPR` to an assignment expression and
                    // record the temp for later inlining into the ternary.
                    for d in decl.declarations.drain(..) {
                        if let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                            let name = id.name.to_string();
                            if let Some(init) = d.init {
                                temp_decls.insert(name.clone(), init.clone_in(cx.allocator));
                                // Emit as assignment expression for sequence.
                                let assign = cx.builder.expression_assignment(
                                    SPAN,
                                    AssignmentOperator::Assign,
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&name),
                                            ),
                                    ),
                                    init,
                                );
                                setup_exprs.push(assign);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Now stmts[i] is the ternary statement. Replace the test.
        if let ast::Statement::ExpressionStatement(es) = &mut stmts[i]
            && let ast::Expression::ConditionalExpression(cond) = &mut es.expression
        {
            // Replace test identifier with the real test expression.
            cond.test = real_test;

            // Check if the consequent is a temp identifier that was declared in setup.
            // Pattern: `x = []; let t1 = EXPR; x = []; t0 ? t1 : ALT`
            // → split setup at t1 decl, inline t1, setup1 → consequent, setup2 → alternate
            let cons_temp_name = if let ast::Expression::Identifier(id) = &cond.consequent {
                let n = id.name.to_string();
                if n.starts_with('t') && n.len() > 1 && n[1..].chars().all(|c| c.is_ascii_digit()) {
                    Some(n)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(ref temp_name) = cons_temp_name
                && let Some(temp_expr) = temp_decls.get(temp_name)
            {
                // The consequent is a temp reference. Replace it with the temp's value.
                cond.consequent = temp_expr.clone_in(cx.allocator);

                // Split setup_exprs at the temp's assignment.
                // Everything before (including the temp assignment) → consequent setup
                // Everything after → alternate setup
                let split_pos = setup_exprs
                    .iter()
                    .position(|expr| {
                        if let ast::Expression::AssignmentExpression(assign) = expr
                            && let ast::AssignmentTarget::AssignmentTargetIdentifier(id) =
                                &assign.left
                        {
                            id.name.as_str() == temp_name.as_str()
                        } else {
                            false
                        }
                    })
                    .map(|p| p + 1)
                    .unwrap_or(setup_exprs.len());

                let alt_setup: Vec<ast::Expression<'a>> = setup_exprs.drain(split_pos..).collect();
                // Remove the temp assignment itself from consequent setup
                // (it's been inlined into the consequent)
                if let Some(ast::Expression::AssignmentExpression(assign)) = setup_exprs.last()
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(id) = &assign.left
                    && id.name.as_str() == temp_name.as_str()
                {
                    setup_exprs.pop();
                }

                // Build consequent sequence: (setup1, CONS_EXPR)
                if !setup_exprs.is_empty() {
                    let cons = std::mem::replace(
                        &mut cond.consequent,
                        cx.builder.expression_null_literal(SPAN),
                    );
                    setup_exprs.push(cons);
                    cond.consequent = cx
                        .builder
                        .expression_sequence(SPAN, cx.builder.vec_from_iter(setup_exprs));
                }

                // Build alternate sequence: (setup2, ALT_EXPR)
                if !alt_setup.is_empty() {
                    let alt = std::mem::replace(
                        &mut cond.alternate,
                        cx.builder.expression_null_literal(SPAN),
                    );
                    let mut alt_seq = alt_setup;
                    alt_seq.push(alt);
                    cond.alternate = cx
                        .builder
                        .expression_sequence(SPAN, cx.builder.vec_from_iter(alt_seq));
                }

                // Skip normal fusion since we handled it here.
                // Don't increment i — process same position again.
                continue;
            }

            // Fuse setup expressions into the consequent as a sequence.
            if !setup_exprs.is_empty() {
                let original_consequent = std::mem::replace(
                    &mut cond.consequent,
                    cx.builder.expression_null_literal(SPAN),
                );
                setup_exprs.push(original_consequent);
                cond.consequent = cx
                    .builder
                    .expression_sequence(SPAN, cx.builder.vec_from_iter(setup_exprs));
            }
        }

        // Don't increment i — process the same position again in case of chaining.
    }
}

const STRUCTURAL_CHECK_IDENT: &str = "$structuralCheck";

/// Format the scope location string for $structuralCheck diagnostics.
/// Returns "(startLine:endLine)" based on scope declaration/instruction locations.
fn format_change_detection_scope_loc(
    scope: &ReactiveScope,
    instructions: &ReactiveBlock,
) -> String {
    let mut lines: Vec<u32> = Vec::new();
    for decl in scope.declarations.values() {
        if let SourceLocation::Source(range) = &decl.identifier.loc {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
    for reassign in &scope.reassignments {
        if let SourceLocation::Source(range) = &reassign.loc {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
    // Also include instruction locations.
    collect_block_lines(instructions, &mut lines);
    if lines.is_empty() {
        "unknown location".to_string()
    } else {
        let min = lines.iter().min().unwrap();
        let max = lines.iter().max().unwrap();
        format!("({min}:{max})")
    }
}

fn collect_block_lines(block: &ReactiveBlock, lines: &mut Vec<u32>) {
    for stmt in block {
        if let ReactiveStatement::Instruction(instr) = stmt
            && let SourceLocation::Source(range) = &instr.loc
        {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
}

/// Build a `$structuralCheck(old, new, "name", "fnName", "phase", "loc")` call statement.
fn build_structural_check_call<'a>(
    cx: &mut CodegenContext<'a>,
    old_expr_name: &str,
    new_expr_name: &str,
    var_name: &str,
    phase: &str,
    loc: &str,
) -> ast::Statement<'a> {
    let fn_name = cx.fn_name.clone();
    let old_expr = if old_expr_name.starts_with("$[") {
        // Cache access expression like $[0]
        let slot: u32 = old_expr_name[2..old_expr_name.len() - 1]
            .parse()
            .unwrap_or(0);
        cx.cache_access(slot)
    } else {
        cx.ident_expr(old_expr_name)
    };
    let new_expr = cx.ident_expr(new_expr_name);

    let mut args = cx.builder.vec_with_capacity(6);
    args.push(ast::Argument::from(old_expr));
    args.push(ast::Argument::from(new_expr));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(var_name),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(&fn_name),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(phase),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(loc),
        None,
    )));

    cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(STRUCTURAL_CHECK_IDENT)),
            NONE,
            args,
            false,
        ),
    )
}

// ---------------------------------------------------------------------------
// Function expression fallback via ReactiveFunction
// ---------------------------------------------------------------------------

/// Build a function expression by constructing a ReactiveFunction from the HIR
/// and using codegen_ast's own codegen. Handles cases where hir_to_ast fails.
fn lower_function_expression_via_reactive<'a>(
    cx: &mut CodegenContext<'a>,
    name: Option<&str>,
    lowered_func: &crate::hir::types::LoweredFunction,
    expr_type: FunctionExpressionType,
) -> Option<ast::Expression<'a>> {
    let hir_func = &lowered_func.func;
    let mut reactive_fn =
        crate::reactive_scopes::build_reactive_function::build_reactive_function(hir_func.clone());

    // Run the same post-reactive passes as the string codegen:
    // - prune_unused_labels: remove unreferenced labels
    // - prune_unused_lvalues: set lvalue=None on instructions whose temp
    //   result is never referenced, so they emit as expression statements
    //   instead of being silently inlined into the temp map
    // - prune_hoisted_contexts: remove unnecessary context hoisting
    crate::reactive_scopes::prune_unused_labels_reactive::prune_unused_labels(&mut reactive_fn);
    crate::reactive_scopes::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn);
    let _ =
        crate::reactive_scopes::prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn);

    // Mark all terminal labels as implicit for inner functions — these are
    // CFG block IDs that shouldn't be emitted as visible JavaScript labels.
    suppress_labels_recursive(&mut reactive_fn.body);

    let options = CodegenOptions {
        enable_change_variable_codegen: false,
        enable_emit_hook_guards: false,
        enable_change_detection_for_debugging: false,
        enable_reset_cache_on_source_file_changes: false,
        fast_refresh_source_hash: None,
        disable_memoization_features: false,
        disable_memoization_for_debugging: false,
        fbt_operands: HashSet::new(),
        cache_binding_name: None,
        unique_identifiers: HashSet::new(),
    };

    let result = codegen_reactive_function(cx.builder, cx.allocator, &reactive_fn, options);

    let mut directives = cx.builder.vec();
    let directives_empty = hir_func.directives.is_empty();
    for directive in &hir_func.directives {
        directives.push(
            cx.builder.directive(
                SPAN,
                cx.builder
                    .string_literal(SPAN, cx.builder.atom(directive), None),
                cx.builder.atom(directive),
            ),
        );
    }

    let body = cx
        .builder
        .alloc(cx.builder.function_body(SPAN, directives, result.body));

    // Build params from the reactive function's param list.
    let mut param_items = cx.builder.vec();
    let mut rest_param = None;
    for param in &reactive_fn.params {
        let (place, is_spread) = match param {
            Argument::Place(p) => (p, false),
            Argument::Spread(p) => (p, true),
        };
        let param_name = place
            .identifier
            .name
            .as_ref()
            .map(|n| n.value().to_string())
            .unwrap_or_else(|| format!("t{}", place.identifier.id.0));
        let pattern = cx
            .builder
            .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&param_name));
        if is_spread {
            rest_param = Some(cx.builder.alloc_formal_parameter_rest(
                SPAN,
                cx.builder.vec(),
                cx.builder.binding_rest_element(SPAN, pattern),
                NONE,
            ));
        } else {
            param_items.push(cx.builder.plain_formal_parameter(SPAN, pattern));
        }
    }
    let params = cx.builder.formal_parameters(
        SPAN,
        ast::FormalParameterKind::FormalParameter,
        param_items,
        rest_param,
    );

    match expr_type {
        FunctionExpressionType::ArrowFunctionExpression => {
            // Detect single-return-expression bodies and convert to expression arrows.
            let (is_expression, body) = if directives_empty
                && body.statements.len() == 1
                && matches!(body.statements[0], ast::Statement::ReturnStatement(_))
            {
                if let ast::Statement::ReturnStatement(ret) = &body.statements[0] {
                    if let Some(arg) = &ret.argument {
                        let expr = arg.clone_in(cx.allocator);
                        let expr_body = cx.builder.alloc(cx.builder.function_body(
                            SPAN,
                            cx.builder.vec(),
                            cx.builder.vec1(cx.builder.statement_expression(SPAN, expr)),
                        ));
                        (true, expr_body)
                    } else {
                        (false, body)
                    }
                } else {
                    (false, body)
                }
            } else {
                (false, body)
            };
            Some(cx.builder.expression_arrow_function(
                SPAN,
                is_expression,
                hir_func.async_,
                NONE,
                cx.builder.alloc(params),
                NONE,
                body,
            ))
        }
        _ => Some(cx.builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            name.map(|n| cx.builder.binding_identifier(SPAN, cx.builder.atom(n))),
            hir_func.generator,
            hir_func.async_,
            false,
            NONE,
            NONE,
            cx.builder.alloc(params),
            NONE,
            Some(body),
        )),
    }
}

// ---------------------------------------------------------------------------
// Hook guard helpers
// ---------------------------------------------------------------------------

const HOOK_GUARD_IDENT: &str = "$dispatcherGuard";
const HOOK_GUARD_ALLOW: u8 = 2;
const HOOK_GUARD_DISALLOW: u8 = 3;

fn is_hook_name_str(name: &str) -> bool {
    name.len() >= 4 && name.starts_with("use") && name.as_bytes()[3].is_ascii_uppercase()
}

/// Check if a resolved callee expression is a hook name (identifier starting with `use` + uppercase).
fn expr_is_hook_name(expr: &ast::Expression<'_>) -> bool {
    match expr {
        ast::Expression::Identifier(ident) => is_hook_name_str(ident.name.as_str()),
        _ => false,
    }
}

/// Check if a method call callee expression has a hook-name property.
/// E.g., `ObjectWithHooks.useIdentity` → checks "useIdentity".
fn method_property_is_hook_name(callee_expr: &ast::Expression<'_>) -> bool {
    match callee_expr {
        ast::Expression::StaticMemberExpression(member) => {
            is_hook_name_str(member.property.name.as_str())
        }
        _ => false,
    }
}

/// Wrap a hook call expression in an IIFE with dispatcher guard:
/// ```js
/// (function() { try { $dispatcherGuard(2); return hookCall(); } finally { $dispatcherGuard(3); } })()
/// ```
fn wrap_hook_guard_iife<'a>(
    cx: &mut CodegenContext<'a>,
    call_expr: ast::Expression<'a>,
) -> ast::Expression<'a> {
    cx.emitted_hook_guards = true;
    cx.needs_function_hook_guard_wrapper = true;

    let guard_allow = cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(HOOK_GUARD_IDENT)),
            NONE,
            cx.builder
                .vec1(ast::Argument::from(cx.builder.expression_numeric_literal(
                    SPAN,
                    HOOK_GUARD_ALLOW as f64,
                    None,
                    NumberBase::Decimal,
                ))),
            false,
        ),
    );

    let return_stmt = cx.builder.statement_return(SPAN, Some(call_expr));

    let guard_disallow = cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(HOOK_GUARD_IDENT)),
            NONE,
            cx.builder
                .vec1(ast::Argument::from(cx.builder.expression_numeric_literal(
                    SPAN,
                    HOOK_GUARD_DISALLOW as f64,
                    None,
                    NumberBase::Decimal,
                ))),
            false,
        ),
    );

    let try_body = cx.builder.vec_from_iter([guard_allow, return_stmt]);

    let try_stmt = cx.builder.statement_try(
        SPAN,
        cx.builder.alloc_block_statement(SPAN, try_body),
        Option::<oxc_allocator::Box<'_, ast::CatchClause<'_>>>::None,
        Some(
            cx.builder
                .alloc_block_statement(SPAN, cx.builder.vec1(guard_disallow)),
        ),
    );

    let function_expr =
        cx.builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            None,
            false,
            false,
            false,
            NONE,
            NONE,
            cx.builder.alloc(cx.builder.formal_parameters(
                SPAN,
                ast::FormalParameterKind::FormalParameter,
                cx.builder.vec(),
                Option::<oxc_allocator::Box<'_, ast::FormalParameterRest<'_>>>::None,
            )),
            NONE,
            Some(cx.builder.alloc(cx.builder.function_body(
                SPAN,
                cx.builder.vec(),
                cx.builder.vec1(try_stmt),
            ))),
        );

    cx.builder
        .expression_call(SPAN, function_expr, NONE, cx.builder.vec(), false)
}
