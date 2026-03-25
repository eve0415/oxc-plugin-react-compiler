use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_span::SPAN;
use oxc_syntax::{
    identifier::is_identifier_name,
    number::NumberBase,
    operator::{
        AssignmentOperator, BinaryOperator as AstBinaryOperator,
        LogicalOperator as AstLogicalOperator, UnaryOperator as AstUnaryOperator,
        UpdateOperator as AstUpdateOperator,
    },
};

use crate::hir::types::{
    self, HIRFunction, IdentifierId, Instruction, InstructionKind, InstructionValue, Place,
    PrimitiveValue, Terminal,
};

pub(crate) fn try_lower_function_body_ast<'a>(
    builder: AstBuilder<'a>,
    hir_function: &HIRFunction,
) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
    let instruction_map = hir_function
        .body
        .blocks
        .iter()
        .flat_map(|(_, block)| block.instructions.iter())
        .map(|instruction| (instruction.lvalue.identifier.id, instruction))
        .collect::<HashMap<_, _>>();
    let used_temps = collect_used_temps(hir_function);
    if hir_function.body.blocks.iter().any(|(_, block)| {
        matches!(
            block.terminal,
            Terminal::Label { .. } | Terminal::Switch { .. }
        ) || block.instructions.iter().any(|instruction| {
            matches!(
                instruction.value,
                InstructionValue::TypeCastExpression { .. }
                    | InstructionValue::StoreContext {
                        lvalue: types::LValue {
                            kind: InstructionKind::Reassign,
                            ..
                        },
                        ..
                    }
            ) || is_self_referential_property_store(instruction, &instruction_map)
                || is_assignment_value_sensitive_store(instruction, &used_temps)
        })
    }) || has_reassign_read_sequence(hir_function, &used_temps)
    {
        return None;
    }

    let block_map = hir_function
        .body
        .blocks
        .iter()
        .map(|(id, block)| (*id, block))
        .collect::<HashMap<_, _>>();
    let synthetic_param_names = synthetic_param_names(hir_function);
    let state = LoweringState::new(
        builder,
        &block_map,
        &instruction_map,
        used_temps,
        synthetic_param_names,
    );
    let body =
        state.lower_block_sequence(hir_function.body.entry, None, &mut HashSet::new(), None)?;
    Some(strip_trailing_empty_return(body))
}

pub(crate) fn try_lower_function_declaration_ast<'a>(
    builder: AstBuilder<'a>,
    hir_function: &HIRFunction,
) -> Option<ast::Statement<'a>> {
    let name = hir_function.id.as_deref()?;
    let synthetic_param_names = synthetic_param_names(hir_function);
    let params = lower_function_params(builder, hir_function, &synthetic_param_names)?;
    let body = try_lower_function_body_ast(builder, hir_function)?;
    let mut directives = builder.vec();
    for directive in &hir_function.directives {
        directives.push(builder.directive(
            SPAN,
            builder.string_literal(SPAN, builder.atom(directive), None),
            builder.atom(directive),
        ));
    }
    let declaration = builder.declaration_function(
        SPAN,
        ast::FunctionType::FunctionDeclaration,
        Some(builder.binding_identifier(SPAN, builder.atom(name))),
        hir_function.generator,
        hir_function.async_,
        false,
        NONE,
        NONE,
        builder.alloc(params),
        NONE,
        Some(builder.alloc(builder.function_body(SPAN, directives, body))),
    );
    match declaration {
        ast::Declaration::FunctionDeclaration(function) => {
            Some(ast::Statement::FunctionDeclaration(function))
        }
        _ => None,
    }
}

fn lower_function_params<'a>(
    builder: AstBuilder<'a>,
    hir_function: &HIRFunction,
    synthetic_param_names: &HashMap<IdentifierId, String>,
) -> Option<ast::FormalParameters<'a>> {
    let mut items = builder.vec();
    let mut rest = None;

    for param in &hir_function.params {
        let (place, is_spread) = match param {
            types::Argument::Place(place) => (place, false),
            types::Argument::Spread(place) => (place, true),
        };
        let name = lowered_place_name(place, synthetic_param_names)?;
        let pattern = builder.binding_pattern_binding_identifier(SPAN, builder.ident(name));
        if is_spread {
            rest = Some(builder.alloc_formal_parameter_rest(
                SPAN,
                builder.vec(),
                builder.binding_rest_element(SPAN, pattern),
                NONE,
            ));
        } else {
            items.push(builder.plain_formal_parameter(SPAN, pattern));
        }
    }

    Some(builder.formal_parameters(SPAN, ast::FormalParameterKind::FormalParameter, items, rest))
}

struct LoweringState<'a, 'hir> {
    builder: AstBuilder<'a>,
    block_map: &'hir HashMap<types::BlockId, &'hir types::BasicBlock>,
    instruction_map: &'hir HashMap<IdentifierId, &'hir Instruction>,
    used_temps: HashSet<IdentifierId>,
    synthetic_param_names: HashMap<IdentifierId, String>,
}

#[derive(Clone, Copy)]
struct ControlContext {
    continue_target: Option<types::BlockId>,
    break_target: Option<types::BlockId>,
}

impl<'a, 'hir> LoweringState<'a, 'hir> {
    fn new(
        builder: AstBuilder<'a>,
        block_map: &'hir HashMap<types::BlockId, &'hir types::BasicBlock>,
        instruction_map: &'hir HashMap<IdentifierId, &'hir Instruction>,
        used_temps: HashSet<IdentifierId>,
        synthetic_param_names: HashMap<IdentifierId, String>,
    ) -> Self {
        Self {
            builder,
            block_map,
            instruction_map,
            used_temps,
            synthetic_param_names,
        }
    }

    fn is_jsx_text_place(&self, place: &Place) -> bool {
        self.instruction_map
            .get(&place.identifier.id)
            .is_some_and(|instr| matches!(instr.value, InstructionValue::JSXText { .. }))
    }

    fn lower_terminal(
        &self,
        terminal: &Terminal,
        stop_at: Option<types::BlockId>,
        visiting_blocks: &mut HashSet<types::BlockId>,
        control_context: Option<ControlContext>,
    ) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
        match terminal {
            Terminal::Return { value, .. } => {
                let value = self.lower_place(value, &mut HashSet::new())?;
                let argument = if is_undefined_expression(&value) {
                    None
                } else {
                    Some(value)
                };
                Some(
                    self.builder
                        .vec1(self.builder.statement_return(SPAN, argument)),
                )
            }
            Terminal::Throw { value, .. } => Some(
                self.builder.vec1(
                    self.builder
                        .statement_throw(SPAN, self.lower_place(value, &mut HashSet::new())?),
                ),
            ),
            Terminal::Goto { block, .. } => {
                if let Some(control_context) = control_context {
                    if control_context.continue_target == Some(*block) {
                        return Some(
                            self.builder
                                .vec1(self.builder.statement_continue(SPAN, None)),
                        );
                    }
                    if control_context.break_target == Some(*block) {
                        return Some(self.builder.vec1(self.builder.statement_break(SPAN, None)));
                    }
                }
                if Some(*block) == stop_at {
                    Some(self.builder.vec())
                } else {
                    None
                }
            }
            Terminal::If {
                test,
                consequent,
                alternate,
                fallthrough,
                ..
            }
            | Terminal::Branch {
                test,
                consequent,
                alternate,
                fallthrough,
                ..
            } => {
                let consequent = self.lower_block_sequence(
                    *consequent,
                    Some(*fallthrough),
                    &mut visiting_blocks.clone(),
                    control_context,
                )?;
                let consequent = self.wrap_block(consequent);
                let alternate = if *alternate == *fallthrough {
                    None
                } else {
                    let alternate = self.lower_block_sequence(
                        *alternate,
                        Some(*fallthrough),
                        &mut visiting_blocks.clone(),
                        control_context,
                    )?;
                    if alternate.is_empty() {
                        None
                    } else {
                        Some(self.wrap_block(alternate))
                    }
                };
                let mut statements = self.builder.vec1(self.builder.statement_if(
                    SPAN,
                    self.lower_place(test, &mut HashSet::new())?,
                    consequent,
                    alternate,
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::While {
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                let test_expr = self.lower_test_block_expr(*test)?;
                let mut body = self.lower_block_sequence(
                    *loop_block,
                    Some(*test),
                    &mut visiting_blocks.clone(),
                    Some(ControlContext {
                        continue_target: Some(*test),
                        break_target: Some(*fallthrough),
                    }),
                )?;
                trim_trailing_loop_continue(&mut body);
                let mut statements = self.builder.vec1(self.builder.statement_while(
                    SPAN,
                    test_expr,
                    self.wrap_block(body),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::DoWhile {
                loop_block,
                test,
                fallthrough,
                ..
            } => {
                let test_expr = self.lower_test_block_expr(*test)?;
                let mut body = self.lower_block_sequence(
                    *loop_block,
                    Some(*test),
                    &mut visiting_blocks.clone(),
                    Some(ControlContext {
                        continue_target: Some(*test),
                        break_target: Some(*fallthrough),
                    }),
                )?;
                trim_trailing_loop_continue(&mut body);
                let mut statements = self.builder.vec1(self.builder.statement_do_while(
                    SPAN,
                    self.wrap_block(body),
                    test_expr,
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::For {
                init,
                test,
                update,
                loop_block,
                fallthrough,
                ..
            } => {
                let init = self.lower_for_init(*init, *test)?;
                let test_expr = self.lower_test_block_expr(*test)?;
                let update_expr = if let Some(update) = update {
                    self.lower_for_update(*update, *test)?
                } else {
                    None
                };
                let continue_target = update.unwrap_or(*test);
                let mut body = self.lower_block_sequence(
                    *loop_block,
                    Some(continue_target),
                    &mut visiting_blocks.clone(),
                    Some(ControlContext {
                        continue_target: Some(continue_target),
                        break_target: Some(*fallthrough),
                    }),
                )?;
                trim_trailing_loop_continue(&mut body);
                let mut statements = self.builder.vec1(self.builder.statement_for(
                    SPAN,
                    init,
                    Some(test_expr),
                    update_expr,
                    self.wrap_block(body),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::ForOf {
                init,
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                let right = self.lower_for_of_right(*init)?;
                let left = self.lower_for_in_of_left(*test)?;
                let mut body = self.lower_block_sequence(
                    *loop_block,
                    Some(*init),
                    &mut visiting_blocks.clone(),
                    Some(ControlContext {
                        continue_target: Some(*init),
                        break_target: Some(*fallthrough),
                    }),
                )?;
                trim_trailing_loop_continue(&mut body);
                let mut statements = self.builder.vec1(ast::Statement::ForOfStatement(
                    self.builder.alloc_for_of_statement(
                        SPAN,
                        false,
                        left,
                        right,
                        self.wrap_block(body),
                    ),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::ForIn {
                init,
                loop_block,
                fallthrough,
                ..
            } => {
                let right = self.lower_for_in_right(*init)?;
                let left = self.lower_for_in_of_left(*init)?;
                let mut body = self.lower_block_sequence(
                    *loop_block,
                    Some(*init),
                    &mut visiting_blocks.clone(),
                    Some(ControlContext {
                        continue_target: Some(*init),
                        break_target: Some(*fallthrough),
                    }),
                )?;
                trim_trailing_loop_continue(&mut body);
                let mut statements = self.builder.vec1(ast::Statement::ForInStatement(
                    self.builder
                        .alloc_for_in_statement(SPAN, left, right, self.wrap_block(body)),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::Switch {
                test,
                cases,
                fallthrough,
                ..
            } => {
                let mut lowered_cases = Vec::with_capacity(cases.len());
                for (index, case) in cases.iter().enumerate() {
                    let next_block = cases
                        .get(index + 1)
                        .map(|next_case| next_case.block)
                        .or(Some(*fallthrough));
                    let consequent = self.lower_block_sequence(
                        case.block,
                        next_block,
                        &mut visiting_blocks.clone(),
                        Some(ControlContext {
                            continue_target: None,
                            break_target: Some(*fallthrough),
                        }),
                    )?;
                    let test = match case.test.as_ref() {
                        Some(place) => Some(self.lower_place(place, &mut HashSet::new())?),
                        None => None,
                    };
                    lowered_cases.push(self.builder.switch_case(SPAN, test, consequent));
                }
                let mut statements = self.builder.vec1(self.builder.statement_switch(
                    SPAN,
                    self.lower_place(test, &mut HashSet::new())?,
                    self.builder.vec_from_iter(lowered_cases),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::Try {
                block,
                handler_binding,
                handler,
                fallthrough,
                ..
            } => {
                let try_body = self.lower_block_sequence(
                    *block,
                    Some(*fallthrough),
                    &mut visiting_blocks.clone(),
                    control_context,
                )?;
                let handler_body = self.lower_block_sequence(
                    *handler,
                    Some(*fallthrough),
                    &mut visiting_blocks.clone(),
                    control_context,
                )?;
                let catch_param = handler_binding
                    .as_ref()
                    .and_then(|binding| self.lower_catch_parameter(binding));
                let mut statements = self.builder.vec1(ast::Statement::TryStatement(
                    self.builder.alloc_try_statement(
                        SPAN,
                        self.builder.alloc_block_statement(SPAN, try_body),
                        Some(self.builder.alloc_catch_clause(
                            SPAN,
                            catch_param,
                            self.builder.alloc_block_statement(SPAN, handler_body),
                        )),
                        None::<oxc_allocator::Box<'a, ast::BlockStatement<'a>>>,
                    ),
                ));
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::Sequence {
                block, fallthrough, ..
            }
            | Terminal::Scope {
                block, fallthrough, ..
            }
            | Terminal::PrunedScope {
                block, fallthrough, ..
            }
            | Terminal::Label {
                block, fallthrough, ..
            } => {
                let mut statements = self.lower_block_sequence(
                    *block,
                    Some(*fallthrough),
                    visiting_blocks,
                    control_context,
                )?;
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::Logical {
                test, fallthrough, ..
            }
            | Terminal::Optional {
                test, fallthrough, ..
            }
            | Terminal::Ternary {
                test, fallthrough, ..
            } => {
                let mut statements = self.lower_block_sequence(
                    *test,
                    Some(*fallthrough),
                    visiting_blocks,
                    control_context,
                )?;
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        control_context,
                    )?);
                }
                Some(statements)
            }
            Terminal::Unreachable { .. } => Some(self.builder.vec()),
            _ => None,
        }
    }

    fn lower_block_sequence(
        &self,
        block_id: types::BlockId,
        stop_at: Option<types::BlockId>,
        visiting_blocks: &mut HashSet<types::BlockId>,
        control_context: Option<ControlContext>,
    ) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
        if Some(block_id) == stop_at {
            return Some(self.builder.vec());
        }
        if !visiting_blocks.insert(block_id) {
            return None;
        }
        let block = self.block_map.get(&block_id)?;
        if !block.phis.is_empty() {
            return None;
        }

        let suppressed_declares =
            collect_same_block_suppressed_declare_instruction_ids(&block.instructions);
        let mut statements = self.builder.vec();
        for (index, instruction) in block.instructions.iter().enumerate() {
            if index > 0
                && let Some(statement) = self.lower_reassign_read_expression_statement(
                    &block.instructions[index - 1],
                    instruction,
                )
            {
                statements.push(statement);
                continue;
            }
            if let Some(statement) =
                self.lower_instruction_to_statement(instruction, &suppressed_declares)?
            {
                statements.push(statement);
            }
        }
        statements.extend(self.lower_terminal(
            &block.terminal,
            stop_at,
            visiting_blocks,
            control_context,
        )?);
        visiting_blocks.remove(&block_id);
        Some(statements)
    }

    fn lower_reassign_read_expression_statement(
        &self,
        previous: &Instruction,
        current: &Instruction,
    ) -> Option<ast::Statement<'a>> {
        if current.lvalue.identifier.name.is_some()
            || self.used_temps.contains(&current.lvalue.identifier.id)
        {
            return None;
        }

        let read_place = match &current.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => place,
            _ => return None,
        };

        let reassigned_place = match &previous.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. }
                if lvalue.kind == InstructionKind::Reassign =>
            {
                &lvalue.place
            }
            _ => return None,
        };

        if reassigned_place.identifier.declaration_id != read_place.identifier.declaration_id {
            return None;
        }

        Some(
            self.builder
                .statement_expression(SPAN, self.lower_place(read_place, &mut HashSet::new())?),
        )
    }

    fn lower_instruction_to_statement(
        &self,
        instruction: &Instruction,
        suppressed_declares: &HashSet<types::InstructionId>,
    ) -> Option<Option<ast::Statement<'a>>> {
        match &instruction.value {
            InstructionValue::DeclareLocal { lvalue, .. }
            | InstructionValue::DeclareContext { lvalue, .. } => {
                if lvalue.kind == InstructionKind::Catch {
                    return Some(None);
                }
                if suppressed_declares.contains(&instruction.id) {
                    return Some(None);
                }
                let name = place_name(&lvalue.place)?;
                Some(Some(self.variable_declaration_statement(
                    name,
                    ast::VariableDeclarationKind::Let,
                    None,
                )))
            }
            InstructionValue::StoreLocal { lvalue, value, .. }
            | InstructionValue::StoreContext { lvalue, value, .. } => {
                let value = self.lower_place(value, &mut HashSet::new())?;
                if let Some(name) = place_name(&lvalue.place) {
                    Some(Some(self.lower_named_store(name, lvalue.kind, value)))
                } else {
                    Some(None)
                }
            }
            InstructionValue::Destructure { lvalue, value, .. } => {
                let kind = variable_declaration_kind(lvalue.kind)?;
                let init = self.lower_place(value, &mut HashSet::new())?;
                let pattern = self.lower_binding_pattern(&lvalue.pattern)?;
                Some(Some(self.variable_pattern_declaration_statement(
                    pattern,
                    kind,
                    Some(init),
                )))
            }
            InstructionValue::CallExpression { .. }
            | InstructionValue::MethodCall { .. }
            | InstructionValue::NewExpression { .. }
            | InstructionValue::StoreGlobal { .. }
            | InstructionValue::PropertyStore { .. }
            | InstructionValue::ComputedStore { .. }
            | InstructionValue::PropertyDelete { .. }
            | InstructionValue::ComputedDelete { .. }
            | InstructionValue::PrefixUpdate { .. }
            | InstructionValue::PostfixUpdate { .. } => {
                if instruction.lvalue.identifier.name.is_some()
                    || self.used_temps.contains(&instruction.lvalue.identifier.id)
                {
                    return Some(None);
                }
                Some(Some(self.builder.statement_expression(
                    SPAN,
                    self.lower_instruction_value(&instruction.value, &mut HashSet::new())?,
                )))
            }
            InstructionValue::Debugger { .. } => Some(Some(ast::Statement::DebuggerStatement(
                self.builder.alloc_debugger_statement(SPAN),
            ))),
            _ => {
                if instruction.lvalue.identifier.name.is_some() {
                    None
                } else {
                    Some(None)
                }
            }
        }
    }

    fn lower_named_store(
        &self,
        name: &str,
        kind: InstructionKind,
        value: ast::Expression<'a>,
    ) -> ast::Statement<'a> {
        let declaration_kind = match kind {
            InstructionKind::Const | InstructionKind::HoistedConst => {
                Some(ast::VariableDeclarationKind::Const)
            }
            InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Catch => {
                Some(ast::VariableDeclarationKind::Let)
            }
            InstructionKind::Reassign
            | InstructionKind::Function
            | InstructionKind::HoistedFunction => None,
        };

        if let Some(kind) = declaration_kind {
            let init = if is_undefined_expression(&value) {
                None
            } else {
                Some(value)
            };
            let decl_kind = if init.is_some() {
                kind
            } else {
                ast::VariableDeclarationKind::Let
            };
            self.variable_declaration_statement(name, decl_kind, init)
        } else {
            self.builder.statement_expression(
                SPAN,
                self.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        self.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                self.builder.ident(name),
                            ),
                    ),
                    value,
                ),
            )
        }
    }

    fn variable_declaration_statement(
        &self,
        name: &str,
        kind: ast::VariableDeclarationKind,
        init: Option<ast::Expression<'a>>,
    ) -> ast::Statement<'a> {
        ast::Statement::VariableDeclaration(
            self.builder.alloc_variable_declaration(
                SPAN,
                kind,
                self.builder.vec1(
                    self.builder.variable_declarator(
                        SPAN,
                        kind,
                        self.builder
                            .binding_pattern_binding_identifier(SPAN, self.builder.ident(name)),
                        NONE,
                        init,
                        false,
                    ),
                ),
                false,
            ),
        )
    }

    fn variable_pattern_declaration_statement(
        &self,
        pattern: ast::BindingPattern<'a>,
        kind: ast::VariableDeclarationKind,
        init: Option<ast::Expression<'a>>,
    ) -> ast::Statement<'a> {
        ast::Statement::VariableDeclaration(
            self.builder.alloc_variable_declaration(
                SPAN,
                kind,
                self.builder.vec1(
                    self.builder
                        .variable_declarator(SPAN, kind, pattern, NONE, init, false),
                ),
                false,
            ),
        )
    }

    fn lower_catch_parameter(&self, place: &Place) -> Option<ast::CatchParameter<'a>> {
        let name = lowered_place_name(place, &self.synthetic_param_names)?;
        Some(
            self.builder.catch_parameter(
                SPAN,
                self.builder
                    .binding_pattern_binding_identifier(SPAN, self.builder.ident(name)),
                NONE,
            ),
        )
    }

    fn lower_binding_pattern(&self, pattern: &types::Pattern) -> Option<ast::BindingPattern<'a>> {
        match pattern {
            types::Pattern::Array(pattern) => {
                let mut elements = self.builder.vec();
                let mut rest = None;
                for (index, element) in pattern.items.iter().enumerate() {
                    match element {
                        types::ArrayElement::Place(place) => {
                            elements.push(Some(self.lower_binding_place_pattern(place)?));
                        }
                        types::ArrayElement::Hole => elements.push(None),
                        types::ArrayElement::Spread(place) => {
                            if index + 1 != pattern.items.len() {
                                return None;
                            }
                            rest = Some(self.builder.alloc_binding_rest_element(
                                SPAN,
                                self.lower_binding_place_pattern(place)?,
                            ));
                        }
                    }
                }
                Some(
                    self.builder
                        .binding_pattern_array_pattern(SPAN, elements, rest),
                )
            }
            types::Pattern::Object(pattern) => {
                let mut properties = self.builder.vec();
                let mut rest = None;
                for property in &pattern.properties {
                    match property {
                        types::ObjectPropertyOrSpread::Spread(place) => {
                            rest = Some(self.builder.alloc_binding_rest_element(
                                SPAN,
                                self.lower_binding_place_pattern(place)?,
                            ));
                        }
                        types::ObjectPropertyOrSpread::Property(property) => {
                            let value = self.lower_binding_place_pattern(&property.place)?;
                            let (key, shorthand, computed) =
                                self.lower_binding_property_key(&property.key, &value)?;
                            properties.push(
                                self.builder
                                    .binding_property(SPAN, key, value, shorthand, computed),
                            );
                        }
                    }
                }
                Some(
                    self.builder
                        .binding_pattern_object_pattern(SPAN, properties, rest),
                )
            }
        }
    }

    fn lower_binding_place_pattern(&self, place: &Place) -> Option<ast::BindingPattern<'a>> {
        let name = place_name(place)?;
        Some(
            self.builder
                .binding_pattern_binding_identifier(SPAN, self.builder.ident(name)),
        )
    }

    fn lower_binding_property_key(
        &self,
        key: &types::ObjectPropertyKey,
        value: &ast::BindingPattern<'a>,
    ) -> Option<(ast::PropertyKey<'a>, bool, bool)> {
        match key {
            types::ObjectPropertyKey::Identifier(name) => Some((
                self.builder
                    .property_key_static_identifier(SPAN, self.builder.ident(name)),
                is_binding_identifier_named(value, name),
                false,
            )),
            types::ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
                self.builder
                    .property_key_static_identifier(SPAN, self.builder.ident(name)),
                is_binding_identifier_named(value, name),
                false,
            )),
            types::ObjectPropertyKey::String(name) => Some((
                ast::PropertyKey::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(name),
                    None,
                )),
                false,
                false,
            )),
            types::ObjectPropertyKey::Number(value) => Some((
                ast::PropertyKey::from(self.builder.expression_numeric_literal(
                    SPAN,
                    *value,
                    None,
                    NumberBase::Decimal,
                )),
                false,
                false,
            )),
            types::ObjectPropertyKey::Computed(_) => None,
        }
    }

    fn wrap_block(
        &self,
        statements: oxc_allocator::Vec<'a, ast::Statement<'a>>,
    ) -> ast::Statement<'a> {
        self.builder.statement_block(SPAN, statements)
    }

    fn lower_place(
        &self,
        place: &Place,
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<ast::Expression<'a>> {
        if let Some(name) = lowered_place_name(place, &self.synthetic_param_names) {
            return Some(
                self.builder
                    .expression_identifier(SPAN, self.builder.ident(name)),
            );
        }

        let identifier_id = place.identifier.id;
        if !visiting.insert(identifier_id) {
            return None;
        }
        let instruction = self.instruction_map.get(&identifier_id)?;
        let expression = self.lower_instruction_value(&instruction.value, visiting);
        visiting.remove(&identifier_id);
        expression
    }

    fn lower_instruction_value(
        &self,
        value: &InstructionValue,
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<ast::Expression<'a>> {
        match value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => self.lower_place(place, visiting),
            InstructionValue::StoreLocal { value, .. }
            | InstructionValue::StoreContext { value, .. } => self.lower_place(value, visiting),
            InstructionValue::LoadGlobal { binding, .. } => Some(
                self.builder
                    .expression_identifier(SPAN, self.builder.ident(binding.name())),
            ),
            InstructionValue::MetaProperty { meta, property, .. } => Some(
                self.builder.expression_meta_property(
                    SPAN,
                    self.builder
                        .identifier_name(SPAN, self.builder.ident(meta.as_str())),
                    self.builder
                        .identifier_name(SPAN, self.builder.ident(property.as_str())),
                ),
            ),
            InstructionValue::FunctionExpression {
                name,
                lowered_func,
                expr_type,
                ..
            } => lower_function_expression_ast(
                self.builder,
                name.as_deref(),
                lowered_func,
                *expr_type,
            ),
            InstructionValue::TemplateLiteral {
                quasis, subexprs, ..
            } => {
                let mut expressions = self.builder.vec();
                for expr in subexprs {
                    expressions.push(self.lower_place(expr, visiting)?);
                }
                Some(
                    self.builder.expression_template_literal(
                        SPAN,
                        self.builder.vec_from_iter(quasis.iter().enumerate().map(
                            |(index, quasi)| {
                                self.builder.template_element(
                                    SPAN,
                                    ast::TemplateElementValue {
                                        raw: self.builder.atom(&quasi.raw),
                                        cooked: quasi
                                            .cooked
                                            .as_deref()
                                            .map(|cooked| self.builder.atom(cooked)),
                                    },
                                    index + 1 == quasis.len(),
                                    false,
                                )
                            },
                        )),
                        expressions,
                    ),
                )
            }
            InstructionValue::Primitive { value, .. } => Some(lower_primitive(self.builder, value)),
            InstructionValue::BinaryExpression {
                operator,
                left,
                right,
                ..
            } => Some(self.builder.expression_binary(
                SPAN,
                self.lower_place(left, visiting)?,
                lower_binary_operator(*operator),
                self.lower_place(right, visiting)?,
            )),
            InstructionValue::UnaryExpression {
                operator, value, ..
            } => Some(self.builder.expression_unary(
                SPAN,
                lower_unary_operator(*operator),
                self.lower_place(value, visiting)?,
            )),
            InstructionValue::LogicalExpression {
                operator,
                left,
                right,
                ..
            } => Some(self.builder.expression_logical(
                SPAN,
                self.lower_place(left, visiting)?,
                lower_logical_operator(*operator),
                maybe_parenthesize_jsx(self.builder, self.lower_place(right, visiting)?),
            )),
            InstructionValue::Ternary {
                test,
                consequent,
                alternate,
                ..
            } => Some(self.builder.expression_conditional(
                SPAN,
                self.lower_place(test, visiting)?,
                maybe_parenthesize_jsx(self.builder, self.lower_place(consequent, visiting)?),
                maybe_parenthesize_jsx(self.builder, self.lower_place(alternate, visiting)?),
            )),
            InstructionValue::CallExpression {
                callee,
                args,
                optional,
                ..
            } => Some(self.builder.expression_call(
                SPAN,
                maybe_parenthesize_call_callee(self.builder, self.lower_place(callee, visiting)?),
                NONE,
                self.lower_arguments(args, visiting)?,
                *optional,
            )),
            InstructionValue::NewExpression { callee, args, .. } => {
                Some(self.builder.expression_new(
                    SPAN,
                    self.lower_place(callee, visiting)?,
                    NONE,
                    self.lower_arguments(args, visiting)?,
                ))
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                receiver_optional,
                call_optional,
                ..
            } => {
                let callee = self.lower_method_call_callee(
                    receiver,
                    property,
                    *receiver_optional,
                    visiting,
                )?;
                Some(self.builder.expression_call(
                    SPAN,
                    maybe_parenthesize_call_callee(self.builder, callee),
                    NONE,
                    self.lower_arguments(args, visiting)?,
                    *call_optional,
                ))
            }
            InstructionValue::TypeCastExpression { value, .. } => self.lower_place(value, visiting),
            InstructionValue::ArrayExpression { elements, .. } => Some(
                self.builder
                    .expression_array(SPAN, self.lower_array_elements(elements, visiting)?),
            ),
            InstructionValue::ObjectExpression { properties, .. } => Some(
                self.builder
                    .expression_object(SPAN, self.lower_object_properties(properties, visiting)?),
            ),
            InstructionValue::JsxExpression {
                tag,
                props,
                children,
                ..
            } => lower_jsx_expression(
                self.builder,
                tag,
                props,
                children.as_deref(),
                |place, visiting| self.lower_place(place, visiting),
                |place| self.is_jsx_text_place(place),
                &HashSet::new(),
                visiting,
            ),
            InstructionValue::JsxFragment { children, .. } => lower_jsx_fragment_expression(
                self.builder,
                children,
                |place, visiting| self.lower_place(place, visiting),
                |place| self.is_jsx_text_place(place),
                visiting,
            ),
            InstructionValue::JSXText { value, .. } => Some(
                self.builder
                    .expression_string_literal(SPAN, self.builder.atom(value), None),
            ),
            InstructionValue::PropertyLoad {
                object,
                property,
                optional,
                ..
            } => Some(lower_property_load(
                self.builder,
                object,
                property,
                *optional,
                |place, visiting| self.lower_place(place, visiting),
                visiting,
            )?),
            InstructionValue::ComputedLoad {
                object,
                property,
                optional,
                ..
            } => Some(ast::Expression::from(
                self.builder.member_expression_computed(
                    SPAN,
                    self.lower_place(object, visiting)?,
                    self.lower_place(property, visiting)?,
                    *optional,
                ),
            )),
            InstructionValue::PropertyStore {
                object,
                property,
                value,
                ..
            } => Some(self.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                lower_property_assignment_target(
                    self.builder,
                    object,
                    property,
                    |place, visiting| self.lower_place(place, visiting),
                    visiting,
                )?,
                self.lower_place(value, visiting)?,
            )),
            InstructionValue::ComputedStore {
                object,
                property,
                value,
                ..
            } => Some(self.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                    self.builder.member_expression_computed(
                        SPAN,
                        self.lower_place(object, visiting)?,
                        self.lower_place(property, visiting)?,
                        false,
                    ),
                )),
                self.lower_place(value, visiting)?,
            )),
            InstructionValue::PropertyDelete {
                object, property, ..
            } => Some(self.builder.expression_unary(
                SPAN,
                AstUnaryOperator::Delete,
                lower_property_load(
                    self.builder,
                    object,
                    property,
                    false,
                    |place, visiting| self.lower_place(place, visiting),
                    visiting,
                )?,
            )),
            InstructionValue::ComputedDelete {
                object, property, ..
            } => Some(self.builder.expression_unary(
                SPAN,
                AstUnaryOperator::Delete,
                ast::Expression::from(self.builder.member_expression_computed(
                    SPAN,
                    self.lower_place(object, visiting)?,
                    self.lower_place(property, visiting)?,
                    false,
                )),
            )),
            InstructionValue::StoreGlobal { name, value, .. } => Some(
                self.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        self.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                self.builder.ident(name),
                            ),
                    ),
                    self.lower_place(value, visiting)?,
                ),
            ),
            InstructionValue::PrefixUpdate {
                lvalue, operation, ..
            } => Some(self.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                true,
                self.lower_simple_assignment_target(lvalue, visiting)?,
            )),
            InstructionValue::PostfixUpdate {
                lvalue, operation, ..
            } => Some(self.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                false,
                self.lower_simple_assignment_target(lvalue, visiting)?,
            )),
            _ => None,
        }
    }

    fn lower_method_call_callee(
        &self,
        receiver: &Place,
        property: &Place,
        receiver_optional: bool,
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<ast::Expression<'a>> {
        if let Some(instruction) = self.instruction_map.get(&property.identifier.id) {
            match &instruction.value {
                InstructionValue::PropertyLoad {
                    object,
                    property,
                    optional,
                    ..
                } if same_place(object, receiver) => {
                    return lower_property_load(
                        self.builder,
                        receiver,
                        property,
                        receiver_optional || *optional,
                        |place, visiting| self.lower_place(place, visiting),
                        visiting,
                    );
                }
                InstructionValue::ComputedLoad {
                    object,
                    property,
                    optional,
                    ..
                } if same_place(object, receiver) => {
                    return Some(ast::Expression::from(
                        self.builder.member_expression_computed(
                            SPAN,
                            self.lower_place(receiver, visiting)?,
                            self.lower_place(property, visiting)?,
                            receiver_optional || *optional,
                        ),
                    ));
                }
                _ => {}
            }
        }

        // Upstream invariant: MethodCall::property must be an unpromoted +
        // unmemoized MemberExpression. If the property temp was promoted
        // (memoized into a reactive scope), the PropertyLoad instruction is
        // no longer available for inlining, and we'd emit a computed member
        // access (e.g. `t0[t1](t2)`) which is semantically incorrect.
        // Bail out matching upstream's CodegenReactiveFunction.ts invariant.
        None
    }

    fn lower_arguments(
        &self,
        args: &[types::Argument],
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
        let mut lowered = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                types::Argument::Place(place) => {
                    lowered.push(ast::Argument::from(self.lower_place(place, visiting)?))
                }
                types::Argument::Spread(place) => lowered.push(
                    self.builder
                        .argument_spread_element(SPAN, self.lower_place(place, visiting)?),
                ),
            }
        }
        Some(self.builder.vec_from_iter(lowered))
    }

    fn lower_simple_assignment_target(
        &self,
        place: &Place,
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<ast::SimpleAssignmentTarget<'a>> {
        let expression = self.lower_place(place, visiting)?;
        expression_to_simple_assignment_target(self.builder, expression)
    }

    fn lower_array_elements(
        &self,
        elements: &[types::ArrayElement],
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<oxc_allocator::Vec<'a, ast::ArrayExpressionElement<'a>>> {
        let mut lowered = Vec::with_capacity(elements.len());
        for element in elements {
            let element = match element {
                types::ArrayElement::Place(place) => {
                    ast::ArrayExpressionElement::from(self.lower_place(place, visiting)?)
                }
                types::ArrayElement::Spread(place) => {
                    self.builder.array_expression_element_spread_element(
                        SPAN,
                        self.lower_place(place, visiting)?,
                    )
                }
                types::ArrayElement::Hole => self.builder.array_expression_element_elision(SPAN),
            };
            lowered.push(element);
        }
        Some(self.builder.vec_from_iter(lowered))
    }

    fn lower_object_properties(
        &self,
        properties: &[types::ObjectPropertyOrSpread],
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<oxc_allocator::Vec<'a, ast::ObjectPropertyKind<'a>>> {
        let mut lowered = Vec::with_capacity(properties.len());
        for property in properties {
            let property = match property {
                types::ObjectPropertyOrSpread::Spread(place) => self
                    .builder
                    .object_property_kind_spread_property(SPAN, self.lower_place(place, visiting)?),
                types::ObjectPropertyOrSpread::Property(property) => {
                    if property.type_ != types::ObjectPropertyType::Property {
                        return None;
                    }
                    let value = self.lower_place(&property.place, visiting)?;
                    let (key, shorthand, computed) =
                        self.lower_object_property_key(&property.key, &value, visiting)?;
                    self.builder.object_property_kind_object_property(
                        SPAN,
                        ast::PropertyKind::Init,
                        key,
                        value,
                        false,
                        shorthand,
                        computed,
                    )
                }
            };
            lowered.push(property);
        }
        Some(self.builder.vec_from_iter(lowered))
    }

    fn lower_object_property_key(
        &self,
        key: &types::ObjectPropertyKey,
        value: &ast::Expression<'a>,
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<(ast::PropertyKey<'a>, bool, bool)> {
        match key {
            types::ObjectPropertyKey::Identifier(name) => {
                let shorthand = matches!(
                    value,
                    ast::Expression::Identifier(identifier) if identifier.name == name.as_str()
                );
                Some((
                    self.builder
                        .property_key_static_identifier(SPAN, self.builder.ident(name)),
                    shorthand,
                    false,
                ))
            }
            types::ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
                self.builder
                    .property_key_static_identifier(SPAN, self.builder.ident(name)),
                false,
                false,
            )),
            types::ObjectPropertyKey::String(name) => Some((
                ast::PropertyKey::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(name),
                    None,
                )),
                false,
                false,
            )),
            types::ObjectPropertyKey::Number(value) => Some((
                ast::PropertyKey::from(self.builder.expression_numeric_literal(
                    SPAN,
                    *value,
                    None,
                    NumberBase::Decimal,
                )),
                false,
                false,
            )),
            types::ObjectPropertyKey::Computed(place) => Some((
                ast::PropertyKey::from(self.lower_place(place, visiting)?),
                false,
                true,
            )),
        }
    }

    fn lower_test_block_expr(&self, block_id: types::BlockId) -> Option<ast::Expression<'a>> {
        let block = self.block_map.get(&block_id)?;
        if !block.phis.is_empty() {
            return None;
        }
        if block.instructions.iter().any(|instruction| {
            matches!(
                instruction.value,
                InstructionValue::StoreLocal {
                    lvalue: types::LValue {
                        kind: InstructionKind::Reassign,
                        ..
                    },
                    ..
                } | InstructionValue::StoreContext {
                    lvalue: types::LValue {
                        kind: InstructionKind::Reassign,
                        ..
                    },
                    ..
                }
            )
        }) {
            return None;
        }
        let expr_instruction = block.instructions.iter().rev().find(|instruction| {
            instruction.lvalue.identifier.name.is_none()
                && self
                    .lower_instruction_value(&instruction.value, &mut HashSet::new())
                    .is_some()
        })?;
        self.lower_instruction_value(&expr_instruction.value, &mut HashSet::new())
    }

    fn lower_for_init(
        &self,
        init_block: types::BlockId,
        test_block: types::BlockId,
    ) -> Option<Option<ast::ForStatementInit<'a>>> {
        let statements =
            self.lower_block_sequence(init_block, Some(test_block), &mut HashSet::new(), None)?;
        if statements.is_empty() {
            return Some(None);
        }
        let mut iter = statements.into_iter();
        let first = iter.next()?;
        if iter.len() != 0 {
            return None;
        }
        match first {
            ast::Statement::VariableDeclaration(declaration) => Some(Some(
                ast::ForStatementInit::VariableDeclaration(declaration),
            )),
            ast::Statement::ExpressionStatement(expression_stmt) => {
                Some(Some(ast::ForStatementInit::from(
                    expression_stmt.expression.clone_in(self.builder.allocator),
                )))
            }
            _ => None,
        }
    }

    fn lower_for_update(
        &self,
        update_block: types::BlockId,
        test_block: types::BlockId,
    ) -> Option<Option<ast::Expression<'a>>> {
        let statements =
            self.lower_block_sequence(update_block, Some(test_block), &mut HashSet::new(), None)?;
        let mut expressions: Vec<ast::Expression<'a>> = statements
            .into_iter()
            .filter_map(|statement| statement_to_expression(statement, self.builder.allocator))
            .collect();

        // Upstream includes the block's trailing value expression (e.g., LoadLocal)
        // in the for-update sequence. Our lower_instruction_to_statement suppresses
        // unnamed LoadLocal/LoadContext instructions, so we need to recover the
        // trailing value from the last block before the test block.
        // Traverse the block chain to find the terminal block of the update sequence.
        let mut cur = update_block;
        loop {
            let Some(block) = self.block_map.get(&cur) else {
                break;
            };
            match &block.terminal {
                Terminal::Goto { block: next, .. } if *next == test_block => {
                    // Found the last block before the test. Check for trailing LoadLocal.
                    if let Some(last_instr) = block.instructions.last()
                        && matches!(
                            last_instr.value,
                            InstructionValue::LoadLocal { .. }
                                | InstructionValue::LoadContext { .. }
                        )
                        && last_instr.lvalue.identifier.name.is_none()
                        && let Some(expr) =
                            self.lower_instruction_value(&last_instr.value, &mut HashSet::new())
                    {
                        expressions.push(expr);
                    }
                    break;
                }
                Terminal::Goto { block: next, .. } => {
                    cur = *next;
                }
                _ => break,
            }
        }

        match expressions.len() {
            0 => Some(None),
            1 => expressions.into_iter().next().map(Some),
            _ => Some(Some(self.builder.expression_sequence(
                SPAN,
                self.builder.vec_from_iter(expressions),
            ))),
        }
    }

    fn lower_for_of_right(&self, init_block: types::BlockId) -> Option<ast::Expression<'a>> {
        let block = self.block_map.get(&init_block)?;
        if !block.phis.is_empty() {
            return None;
        }
        let iterator_init = block.instructions.iter().find_map(|instruction| {
            if let InstructionValue::GetIterator { collection, .. } = &instruction.value {
                Some(collection)
            } else {
                None
            }
        })?;
        self.lower_place(iterator_init, &mut HashSet::new())
    }

    fn lower_for_in_right(&self, init_block: types::BlockId) -> Option<ast::Expression<'a>> {
        let block = self.block_map.get(&init_block)?;
        if !block.phis.is_empty() {
            return None;
        }
        let object = block.instructions.iter().find_map(|instruction| {
            if let InstructionValue::NextPropertyOf { value, .. } = &instruction.value {
                Some(value)
            } else {
                None
            }
        })?;
        self.lower_place(object, &mut HashSet::new())
    }

    fn lower_for_in_of_left(&self, block_id: types::BlockId) -> Option<ast::ForStatementLeft<'a>> {
        let block = self.block_map.get(&block_id)?;
        if !block.phis.is_empty() {
            return None;
        }
        for instruction in &block.instructions {
            match &instruction.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    let name = place_name(&lvalue.place)?;
                    let left = if lvalue.kind == InstructionKind::Reassign {
                        ast::ForStatementLeft::from(ast::AssignmentTarget::from(
                            self.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    self.builder.ident(name),
                                ),
                        ))
                    } else {
                        self.builder.for_statement_left_variable_declaration(
                            SPAN,
                            variable_declaration_kind(lvalue.kind)?,
                            self.builder.vec1(self.builder.variable_declarator(
                                SPAN,
                                variable_declaration_kind(lvalue.kind)?,
                                self.builder.binding_pattern_binding_identifier(
                                    SPAN,
                                    self.builder.ident(name),
                                ),
                                NONE,
                                None,
                                false,
                            )),
                            false,
                        )
                    };
                    return Some(left);
                }
                InstructionValue::StoreGlobal { name, .. } => {
                    return Some(ast::ForStatementLeft::from(ast::AssignmentTarget::from(
                        self.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                self.builder.ident(name),
                            ),
                    )));
                }
                InstructionValue::PropertyStore {
                    object, property, ..
                } => {
                    return Some(ast::ForStatementLeft::from(
                        lower_property_assignment_target(
                            self.builder,
                            object,
                            property,
                            |place, visiting| self.lower_place(place, visiting),
                            &mut HashSet::new(),
                        )?,
                    ));
                }
                InstructionValue::ComputedStore {
                    object, property, ..
                } => {
                    return Some(ast::ForStatementLeft::from(ast::AssignmentTarget::from(
                        ast::SimpleAssignmentTarget::from(self.builder.member_expression_computed(
                            SPAN,
                            self.lower_place(object, &mut HashSet::new())?,
                            self.lower_place(property, &mut HashSet::new())?,
                            false,
                        )),
                    )));
                }
                _ => {}
            }
        }
        None
    }
}

pub(crate) fn lower_property_load<'a, F>(
    builder: AstBuilder<'a>,
    object: &Place,
    property: &types::PropertyLiteral,
    optional: bool,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>>,
{
    let object = lower_place(object, visiting)?;
    match property {
        types::PropertyLiteral::String(name) if is_identifier_name(name) => {
            Some(ast::Expression::from(builder.member_expression_static(
                SPAN,
                object,
                builder.identifier_name(SPAN, builder.ident(name)),
                optional,
            )))
        }
        types::PropertyLiteral::String(name) => {
            Some(ast::Expression::from(builder.member_expression_computed(
                SPAN,
                object,
                builder.expression_string_literal(SPAN, builder.atom(name), None),
                optional,
            )))
        }
        types::PropertyLiteral::Number(value) => {
            Some(ast::Expression::from(builder.member_expression_computed(
                SPAN,
                object,
                builder.expression_numeric_literal(SPAN, *value, None, NumberBase::Decimal),
                optional,
            )))
        }
    }
}

pub(crate) fn lower_property_assignment_target<'a, F>(
    builder: AstBuilder<'a>,
    object: &Place,
    property: &types::PropertyLiteral,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::AssignmentTarget<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>>,
{
    let expression = lower_property_load(builder, object, property, false, lower_place, visiting)?;
    expression_to_assignment_target(builder, expression)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_jsx_expression<'a, F, G>(
    builder: AstBuilder<'a>,
    tag: &types::JsxTag,
    props: &[types::JsxAttribute],
    children: Option<&[Place]>,
    lower_place: F,
    is_jsx_text: G,
    fbt_operands: &HashSet<IdentifierId>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    let opening_name = lower_jsx_element_name(builder, tag, lower_place, visiting)?;
    let attributes = lower_jsx_attributes(builder, props, lower_place, fbt_operands, visiting)?;
    let is_single_child_fbt_tag = matches!(tag, types::JsxTag::BuiltinTag(name) if name == "fbt:param" || name == "fbs:param");
    let jsx_children = if is_single_child_fbt_tag {
        lower_jsx_fbt_children(
            builder,
            children.unwrap_or(&[]),
            lower_place,
            is_jsx_text,
            visiting,
        )?
    } else {
        lower_jsx_children(
            builder,
            children.unwrap_or(&[]),
            lower_place,
            is_jsx_text,
            visiting,
        )?
    };
    let closing_element = if jsx_children.is_empty() {
        None
    } else {
        // Clone the opening name for the closing element to avoid
        // double-consuming the tag Place's temp expression.
        let closing_name = opening_name.clone_in(builder.allocator);
        Some(builder.alloc_jsx_closing_element(SPAN, closing_name))
    };

    Some(builder.expression_jsx_element(
        SPAN,
        builder.alloc_jsx_opening_element(SPAN, opening_name, NONE, attributes),
        jsx_children,
        closing_element,
    ))
}

pub(crate) fn lower_jsx_fragment_expression<'a, F, G>(
    builder: AstBuilder<'a>,
    children: &[Place],
    lower_place: F,
    is_jsx_text: G,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    Some(builder.expression_jsx_fragment(
        SPAN,
        builder.jsx_opening_fragment(SPAN),
        lower_jsx_children(builder, children, lower_place, is_jsx_text, visiting)?,
        builder.jsx_closing_fragment(SPAN),
    ))
}

fn lower_jsx_element_name<'a, F>(
    builder: AstBuilder<'a>,
    tag: &types::JsxTag,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::JSXElementName<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    match tag {
        types::JsxTag::BuiltinTag(name) => {
            if let Some((namespace, local)) = name.split_once(':') {
                Some(builder.jsx_element_name_namespaced_name(
                    SPAN,
                    builder.jsx_identifier(SPAN, builder.atom(namespace)),
                    builder.jsx_identifier(SPAN, builder.atom(local)),
                ))
            } else {
                Some(builder.jsx_element_name_identifier(SPAN, builder.atom(name)))
            }
        }
        types::JsxTag::Component(place) => {
            expression_to_jsx_element_name(builder, lower_place(place, visiting)?)
        }
    }
}

fn expression_to_jsx_element_name<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::JSXElementName<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => {
            Some(builder.jsx_element_name_identifier_reference(SPAN, identifier.name))
        }
        ast::Expression::StaticMemberExpression(member) => {
            Some(builder.jsx_element_name_member_expression(
                SPAN,
                expression_to_jsx_member_expression_object(
                    builder,
                    member.object.clone_in(builder.allocator),
                )?,
                builder.jsx_identifier(SPAN, member.property.name),
            ))
        }
        ast::Expression::ThisExpression(_) => Some(builder.jsx_element_name_this_expression(SPAN)),
        _ => None,
    }
}

fn expression_to_jsx_member_expression_object<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::JSXMemberExpressionObject<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => {
            Some(builder.jsx_member_expression_object_identifier_reference(SPAN, identifier.name))
        }
        ast::Expression::StaticMemberExpression(member) => {
            Some(builder.jsx_member_expression_object_member_expression(
                SPAN,
                expression_to_jsx_member_expression_object(
                    builder,
                    member.object.clone_in(builder.allocator),
                )?,
                builder.jsx_identifier(SPAN, member.property.name),
            ))
        }
        ast::Expression::ThisExpression(_) => {
            Some(builder.jsx_member_expression_object_this_expression(SPAN))
        }
        _ => None,
    }
}

fn lower_jsx_attributes<'a, F>(
    builder: AstBuilder<'a>,
    props: &[types::JsxAttribute],
    lower_place: F,
    fbt_operands: &HashSet<IdentifierId>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::JSXAttributeItem<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let mut attributes = builder.vec();
    for prop in props {
        match prop {
            types::JsxAttribute::Attribute { name, place } => {
                let value =
                    lower_jsx_attribute_value(builder, place, lower_place, fbt_operands, visiting)?;
                attributes.push(builder.jsx_attribute_item_attribute(
                    SPAN,
                    builder.jsx_attribute_name_identifier(SPAN, builder.atom(name)),
                    value,
                ));
            }
            types::JsxAttribute::SpreadAttribute { argument } => {
                attributes.push(
                    builder.jsx_attribute_item_spread_attribute(
                        SPAN,
                        lower_place(argument, visiting)?,
                    ),
                );
            }
        }
    }
    Some(attributes)
}

fn lower_jsx_attribute_value<'a, F>(
    builder: AstBuilder<'a>,
    place: &Place,
    lower_place: F,
    fbt_operands: &HashSet<IdentifierId>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<Option<ast::JSXAttributeValue<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let expression = lower_place(place, visiting)?;
    match expression {
        ast::Expression::BooleanLiteral(boolean) => {
            let expr = ast::Expression::BooleanLiteral(boolean);
            Some(Some(ast::JSXAttributeValue::ExpressionContainer(
                builder
                    .alloc(builder.jsx_expression_container(SPAN, ast::JSXExpression::from(expr))),
            )))
        }
        ast::Expression::StringLiteral(literal) => {
            // Upstream STRING_REQUIRES_EXPR_CONTAINER_PATTERN:
            // /[\u{0000}-\u{001F}\u{007F}\u{0080}-\u{FFFF}\u{010000}-\u{10FFFF}]|"|\\/
            // Wraps in expression container when the value contains control chars,
            // non-ASCII chars, double quotes, or backslashes — unless it's an fbt operand.
            let is_fbt_operand = fbt_operands.contains(&place.identifier.id);
            let needs_expr_container = !is_fbt_operand
                && literal.value.chars().any(|c| {
                    matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}'..='\u{FFFF}'
                        | '\u{10000}'..='\u{10FFFF}' | '"' | '\\')
                });
            if needs_expr_container {
                let expr = ast::Expression::StringLiteral(literal);
                Some(Some(ast::JSXAttributeValue::ExpressionContainer(
                    builder.alloc(
                        builder.jsx_expression_container(SPAN, ast::JSXExpression::from(expr)),
                    ),
                )))
            } else {
                Some(Some(ast::JSXAttributeValue::StringLiteral(literal)))
            }
        }
        ast::Expression::JSXElement(element) => {
            // Wrap JSX elements in expression containers: value={<Foo/>}
            Some(Some(ast::JSXAttributeValue::ExpressionContainer(
                builder.alloc_jsx_expression_container(
                    SPAN,
                    ast::JSXExpression::from(ast::Expression::JSXElement(element)),
                ),
            )))
        }
        ast::Expression::JSXFragment(fragment) => {
            // Wrap JSX fragments in expression containers: value={<>...</>}
            Some(Some(ast::JSXAttributeValue::ExpressionContainer(
                builder.alloc_jsx_expression_container(
                    SPAN,
                    ast::JSXExpression::from(ast::Expression::JSXFragment(fragment)),
                ),
            )))
        }
        expression => Some(Some(ast::JSXAttributeValue::ExpressionContainer(
            builder.alloc_jsx_expression_container(SPAN, ast::JSXExpression::from(expression)),
        ))),
    }
}

/// Pattern matching upstream's `JSX_TEXT_CHILD_REQUIRES_EXPR_CONTAINER_PATTERN`.
/// When a JSXText value contains any of these characters, it must be wrapped
/// in an expression container to preserve the value through JSX parsing.
fn jsx_text_needs_expr_container(value: &str) -> bool {
    value
        .chars()
        .any(|c| matches!(c, '<' | '>' | '&' | '{' | '}'))
}

fn lower_jsx_children<'a, F, G>(
    builder: AstBuilder<'a>,
    children: &[Place],
    lower_place: F,
    is_jsx_text: G,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::JSXChild<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    let mut lowered = builder.vec();
    for child in children {
        lowered.push(lower_jsx_child(
            builder,
            child,
            lower_place,
            is_jsx_text,
            visiting,
        )?);
    }
    Some(lowered)
}

fn lower_jsx_child<'a, F, G>(
    builder: AstBuilder<'a>,
    place: &Place,
    lower_place: F,
    is_jsx_text: G,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::JSXChild<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    let expression = lower_place(place, visiting)?;
    // Upstream codegenJsxElement distinguishes JSXText from other expressions:
    // - JSXText with <>&{} chars → ExpressionContainer(StringLiteral)
    // - JSXText without those chars → raw JSXText
    // - JSXElement/JSXFragment → passthrough
    // - Everything else → ExpressionContainer(expression)
    if is_jsx_text(place) {
        match expression {
            ast::Expression::StringLiteral(literal) => {
                if jsx_text_needs_expr_container(&literal.value) {
                    Some(builder.jsx_child_expression_container(
                        SPAN,
                        ast::JSXExpression::from(ast::Expression::StringLiteral(literal)),
                    ))
                } else {
                    let value = literal.unbox().value;
                    Some(ast::JSXChild::Text(
                        builder.alloc_jsx_text(SPAN, value, None),
                    ))
                }
            }
            expression => Some(
                builder.jsx_child_expression_container(SPAN, ast::JSXExpression::from(expression)),
            ),
        }
    } else {
        match expression {
            ast::Expression::JSXElement(element) => Some(ast::JSXChild::Element(element)),
            ast::Expression::JSXFragment(fragment) => Some(ast::JSXChild::Fragment(fragment)),
            expression => Some(
                builder.jsx_child_expression_container(SPAN, ast::JSXExpression::from(expression)),
            ),
        }
    }
}

/// Lower JSX children for `fbt:param` / `fbs:param` tags.
///
/// `babel-plugin-fbt` only accepts JSX elements or expression containers as
/// children of `<fbt:param>`. A bare `JSXFragment` child is rejected, so we
/// wrap fragments in a `JSXExpressionContainer` (`{<>...</>}`).
///
/// Upstream `codegenJsxFbtChildElement` keeps JSXText and JSXElement as-is.
fn lower_jsx_fbt_children<'a, F, G>(
    builder: AstBuilder<'a>,
    children: &[Place],
    lower_place: F,
    is_jsx_text: G,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::JSXChild<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    let mut lowered = builder.vec();
    for child in children {
        lowered.push(lower_jsx_fbt_child(
            builder,
            child,
            lower_place,
            is_jsx_text,
            visiting,
        )?);
    }
    Some(lowered)
}

fn lower_jsx_fbt_child<'a, F, G>(
    builder: AstBuilder<'a>,
    place: &Place,
    lower_place: F,
    is_jsx_text: G,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::JSXChild<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
    G: Fn(&Place) -> bool + Copy,
{
    let expression = lower_place(place, visiting)?;
    // Upstream codegenJsxFbtChildElement: JSXText and JSXElement pass through.
    if is_jsx_text(place) {
        match expression {
            ast::Expression::StringLiteral(literal) => {
                let value = literal.unbox().value;
                Some(ast::JSXChild::Text(
                    builder.alloc_jsx_text(SPAN, value, None),
                ))
            }
            expression => Some(
                builder.jsx_child_expression_container(SPAN, ast::JSXExpression::from(expression)),
            ),
        }
    } else {
        match expression {
            // fbt:param only allows JSX element or expression container as children.
            // JSXElement is passed through directly.
            ast::Expression::JSXElement(element) => Some(ast::JSXChild::Element(element)),
            // JSXFragment must be wrapped in an expression container so that
            // babel-plugin-fbt can process it: {<>...</>} rather than bare <>...</>.
            ast::Expression::JSXFragment(fragment) => Some(builder.jsx_child_expression_container(
                SPAN,
                ast::JSXExpression::from(ast::Expression::JSXFragment(fragment)),
            )),
            expression => Some(
                builder.jsx_child_expression_container(SPAN, ast::JSXExpression::from(expression)),
            ),
        }
    }
}

pub(crate) fn lower_function_expression_ast<'a>(
    builder: AstBuilder<'a>,
    name: Option<&str>,
    lowered_func: &types::LoweredFunction,
    expr_type: types::FunctionExpressionType,
) -> Option<ast::Expression<'a>> {
    let hir_function = &lowered_func.func;
    let synthetic_param_names = synthetic_param_names(hir_function);
    let params = lower_function_params(builder, hir_function, &synthetic_param_names)?;
    let statements = try_lower_function_body_ast(builder, hir_function)?;
    let mut directives = builder.vec();
    for directive in &hir_function.directives {
        directives.push(builder.directive(
            SPAN,
            builder.string_literal(SPAN, builder.atom(directive), None),
            builder.atom(directive),
        ));
    }

    if expr_type == types::FunctionExpressionType::ArrowFunctionExpression
        && directives.is_empty()
        && let Some(expression) = single_return_expression(&statements, builder.allocator)
        && !matches!(
            &expression,
            ast::Expression::JSXElement(_) | ast::Expression::JSXFragment(_)
        )
    {
        return Some(builder.expression_arrow_function(
            SPAN,
            true,
            hir_function.async_,
            NONE,
            builder.alloc(params),
            NONE,
            builder.alloc(builder.function_body(
                SPAN,
                builder.vec(),
                builder.vec1(
                    builder.statement_expression(SPAN, maybe_parenthesize_jsx(builder, expression)),
                ),
            )),
        ));
    }

    let body = builder.alloc(builder.function_body(SPAN, directives, statements));
    match expr_type {
        types::FunctionExpressionType::ArrowFunctionExpression => {
            Some(builder.expression_arrow_function(
                SPAN,
                false,
                hir_function.async_,
                NONE,
                builder.alloc(params),
                NONE,
                body,
            ))
        }
        types::FunctionExpressionType::FunctionExpression
        | types::FunctionExpressionType::FunctionDeclaration => Some(builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            name.map(|name| builder.binding_identifier(SPAN, builder.atom(name))),
            hir_function.generator,
            hir_function.async_,
            false,
            NONE,
            NONE,
            builder.alloc(params),
            NONE,
            Some(body),
        )),
    }
}

fn collect_used_temps(hir_function: &HIRFunction) -> HashSet<IdentifierId> {
    let mut used = HashSet::new();
    for (_, block) in &hir_function.body.blocks {
        for instruction in &block.instructions {
            collect_instruction_uses(instruction, &mut used);
        }
        collect_terminal_uses(&block.terminal, &mut used);
    }
    used
}

fn is_self_referential_property_store(
    instruction: &Instruction,
    instruction_map: &HashMap<IdentifierId, &Instruction>,
) -> bool {
    match &instruction.value {
        InstructionValue::PropertyStore {
            object,
            property,
            value,
            ..
        } => {
            let Some(value_instruction) = instruction_map.get(&value.identifier.id) else {
                return false;
            };
            let InstructionValue::BinaryExpression { left, .. } = &value_instruction.value else {
                return false;
            };
            let Some(left_instruction) = instruction_map.get(&left.identifier.id) else {
                return false;
            };
            matches!(
                &left_instruction.value,
                InstructionValue::PropertyLoad {
                    object: left_object,
                    property: left_property,
                    ..
                } if same_place(left_object, object) && left_property == property
            )
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            let Some(value_instruction) = instruction_map.get(&value.identifier.id) else {
                return false;
            };
            let InstructionValue::BinaryExpression { left, .. } = &value_instruction.value else {
                return false;
            };
            let Some(left_instruction) = instruction_map.get(&left.identifier.id) else {
                return false;
            };
            matches!(
                &left_instruction.value,
                InstructionValue::ComputedLoad {
                    object: left_object,
                    property: left_property,
                    ..
                } if same_place(left_object, object) && same_place(left_property, property)
            )
        }
        _ => false,
    }
}

fn is_assignment_value_sensitive_store(
    instruction: &Instruction,
    used_temps: &HashSet<IdentifierId>,
) -> bool {
    matches!(
        instruction.value,
        InstructionValue::StoreLocal {
            lvalue: types::LValue {
                kind: InstructionKind::Reassign,
                ..
            },
            ..
        } | InstructionValue::StoreContext {
            lvalue: types::LValue {
                kind: InstructionKind::Reassign,
                ..
            },
            ..
        }
    ) && used_temps.contains(&instruction.lvalue.identifier.id)
}

fn has_reassign_read_sequence(
    hir_function: &HIRFunction,
    used_temps: &HashSet<IdentifierId>,
) -> bool {
    hir_function.body.blocks.iter().any(|(_, block)| {
        block.instructions.windows(2).any(|pair| {
            let [store_instr, read_instr] = pair else {
                return false;
            };
            let reassigned_decl_id = match &store_instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                    if lvalue.kind == InstructionKind::Reassign =>
                {
                    lvalue.place.identifier.declaration_id
                }
                _ => return false,
            };

            if read_instr.lvalue.identifier.name.is_some()
                || used_temps.contains(&read_instr.lvalue.identifier.id)
            {
                return false;
            }

            match &read_instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    place.identifier.declaration_id == reassigned_decl_id
                }
                _ => false,
            }
        })
    })
}

fn collect_terminal_uses(terminal: &Terminal, used: &mut HashSet<IdentifierId>) {
    match terminal {
        Terminal::Return { value, .. }
        | Terminal::Throw { value, .. }
        | Terminal::If { test: value, .. }
        | Terminal::Branch { test: value, .. }
        | Terminal::Switch { test: value, .. } => {
            record_temp_use(value, used);
        }
        _ => {}
    }
}

fn collect_instruction_uses(instruction: &Instruction, used: &mut HashSet<IdentifierId>) {
    match &instruction.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            record_temp_use(place, used);
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            record_temp_use(value, used);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            record_temp_use(left, used);
            record_temp_use(right, used);
        }
        InstructionValue::UnaryExpression { value, .. }
        | InstructionValue::TypeCastExpression { value, .. } => {
            record_temp_use(value, used);
        }
        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            record_temp_use(callee, used);
            record_argument_uses(args, used);
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            record_temp_use(receiver, used);
            record_temp_use(property, used);
            record_argument_uses(args, used);
        }
        InstructionValue::PropertyLoad { object, .. } => record_temp_use(object, used),
        InstructionValue::PropertyStore { object, value, .. } => {
            record_temp_use(object, used);
            record_temp_use(value, used);
        }
        InstructionValue::PropertyDelete { object, .. } => record_temp_use(object, used),
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            record_temp_use(object, used);
            record_temp_use(property, used);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            record_temp_use(object, used);
            record_temp_use(property, used);
            record_temp_use(value, used);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            record_temp_use(object, used);
            record_temp_use(property, used);
        }
        InstructionValue::StoreGlobal { value, .. } => record_temp_use(value, used),
        InstructionValue::LogicalExpression { left, right, .. } => {
            record_temp_use(left, used);
            record_temp_use(right, used);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            record_temp_use(test, used);
            record_temp_use(consequent, used);
            record_temp_use(alternate, used);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => record_temp_use(lvalue, used),
        _ => {}
    }
}

fn record_argument_uses(args: &[types::Argument], used: &mut HashSet<IdentifierId>) {
    for arg in args {
        match arg {
            types::Argument::Place(place) | types::Argument::Spread(place) => {
                record_temp_use(place, used);
            }
        }
    }
}

fn record_temp_use(place: &Place, used: &mut HashSet<IdentifierId>) {
    if place.identifier.name.is_none() {
        used.insert(place.identifier.id);
    }
}

fn maybe_parenthesize_jsx<'a>(
    builder: AstBuilder<'a>,
    expr: ast::Expression<'a>,
) -> ast::Expression<'a> {
    if matches!(
        &expr,
        ast::Expression::JSXElement(_) | ast::Expression::JSXFragment(_)
    ) {
        builder.expression_parenthesized(SPAN, expr)
    } else {
        expr
    }
}

fn maybe_parenthesize_call_callee<'a>(
    builder: AstBuilder<'a>,
    expr: ast::Expression<'a>,
) -> ast::Expression<'a> {
    if matches!(
        &expr,
        ast::Expression::FunctionExpression(_) | ast::Expression::ArrowFunctionExpression(_)
    ) {
        builder.expression_parenthesized(SPAN, expr)
    } else {
        expr
    }
}

fn is_undefined_expression(expression: &ast::Expression<'_>) -> bool {
    matches!(
        expression,
        ast::Expression::Identifier(identifier) if identifier.name == "undefined"
    )
}

fn trim_trailing_loop_continue(statements: &mut oxc_allocator::Vec<'_, ast::Statement<'_>>) {
    if matches!(
        statements.last(),
        Some(ast::Statement::ContinueStatement(_))
    ) {
        statements.pop();
    }
}

fn strip_trailing_empty_return<'a>(
    mut statements: oxc_allocator::Vec<'a, ast::Statement<'a>>,
) -> oxc_allocator::Vec<'a, ast::Statement<'a>> {
    if matches!(
        statements.last(),
        Some(ast::Statement::ReturnStatement(return_stmt)) if return_stmt.argument.is_none()
    ) {
        statements.pop();
    }
    statements
}

fn statement_to_expression<'a>(
    statement: ast::Statement<'a>,
    allocator: &'a Allocator,
) -> Option<ast::Expression<'a>> {
    match statement {
        ast::Statement::ExpressionStatement(expr_stmt) => {
            Some(expr_stmt.expression.clone_in(allocator))
        }
        _ => None,
    }
}

fn single_return_expression<'a>(
    statements: &oxc_allocator::Vec<'a, ast::Statement<'a>>,
    allocator: &'a Allocator,
) -> Option<ast::Expression<'a>> {
    if statements.len() != 1 {
        return None;
    }
    match statements.first()? {
        ast::Statement::ReturnStatement(return_stmt) => {
            Some(return_stmt.argument.as_ref()?.clone_in(allocator))
        }
        _ => None,
    }
}

pub(crate) fn lower_primitive<'a>(
    builder: AstBuilder<'a>,
    value: &PrimitiveValue,
) -> ast::Expression<'a> {
    match value {
        PrimitiveValue::Null => builder.expression_null_literal(SPAN),
        PrimitiveValue::Undefined => builder.expression_identifier(SPAN, "undefined"),
        PrimitiveValue::Boolean(value) => builder.expression_boolean_literal(SPAN, *value),
        PrimitiveValue::Number(value) => {
            builder.expression_numeric_literal(SPAN, *value, None, NumberBase::Decimal)
        }
        PrimitiveValue::String(value) => {
            builder.expression_string_literal(SPAN, builder.atom(value), None)
        }
    }
}

pub(crate) fn lower_binary_operator(operator: types::BinaryOperator) -> AstBinaryOperator {
    match operator {
        types::BinaryOperator::Eq => AstBinaryOperator::Equality,
        types::BinaryOperator::NotEq => AstBinaryOperator::Inequality,
        types::BinaryOperator::StrictEq => AstBinaryOperator::StrictEquality,
        types::BinaryOperator::StrictNotEq => AstBinaryOperator::StrictInequality,
        types::BinaryOperator::Lt => AstBinaryOperator::LessThan,
        types::BinaryOperator::LtEq => AstBinaryOperator::LessEqualThan,
        types::BinaryOperator::Gt => AstBinaryOperator::GreaterThan,
        types::BinaryOperator::GtEq => AstBinaryOperator::GreaterEqualThan,
        types::BinaryOperator::LShift => AstBinaryOperator::ShiftLeft,
        types::BinaryOperator::RShift => AstBinaryOperator::ShiftRight,
        types::BinaryOperator::URShift => AstBinaryOperator::ShiftRightZeroFill,
        types::BinaryOperator::Add => AstBinaryOperator::Addition,
        types::BinaryOperator::Sub => AstBinaryOperator::Subtraction,
        types::BinaryOperator::Mul => AstBinaryOperator::Multiplication,
        types::BinaryOperator::Div => AstBinaryOperator::Division,
        types::BinaryOperator::Mod => AstBinaryOperator::Remainder,
        types::BinaryOperator::Exp => AstBinaryOperator::Exponential,
        types::BinaryOperator::BitOr => AstBinaryOperator::BitwiseOR,
        types::BinaryOperator::BitXor => AstBinaryOperator::BitwiseXOR,
        types::BinaryOperator::BitAnd => AstBinaryOperator::BitwiseAnd,
        types::BinaryOperator::In => AstBinaryOperator::In,
        types::BinaryOperator::InstanceOf => AstBinaryOperator::Instanceof,
    }
}

pub(crate) fn lower_unary_operator(operator: types::UnaryOperator) -> AstUnaryOperator {
    match operator {
        types::UnaryOperator::Minus => AstUnaryOperator::UnaryNegation,
        types::UnaryOperator::Plus => AstUnaryOperator::UnaryPlus,
        types::UnaryOperator::Not => AstUnaryOperator::LogicalNot,
        types::UnaryOperator::BitNot => AstUnaryOperator::BitwiseNot,
        types::UnaryOperator::TypeOf => AstUnaryOperator::Typeof,
        types::UnaryOperator::Void => AstUnaryOperator::Void,
    }
}

pub(crate) fn lower_logical_operator(operator: types::LogicalOperator) -> AstLogicalOperator {
    match operator {
        types::LogicalOperator::And => AstLogicalOperator::And,
        types::LogicalOperator::Or => AstLogicalOperator::Or,
        types::LogicalOperator::NullishCoalescing => AstLogicalOperator::Coalesce,
    }
}

pub(crate) fn lower_update_operator(operator: types::UpdateOperator) -> AstUpdateOperator {
    match operator {
        types::UpdateOperator::Increment => AstUpdateOperator::Increment,
        types::UpdateOperator::Decrement => AstUpdateOperator::Decrement,
    }
}

pub(crate) fn expression_to_simple_assignment_target<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::SimpleAssignmentTarget<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => Some(
            builder.simple_assignment_target_assignment_target_identifier(SPAN, identifier.name),
        ),
        ast::Expression::ComputedMemberExpression(member) => {
            Some(ast::SimpleAssignmentTarget::from(
                ast::MemberExpression::ComputedMemberExpression(member),
            ))
        }
        ast::Expression::StaticMemberExpression(member) => Some(ast::SimpleAssignmentTarget::from(
            ast::MemberExpression::StaticMemberExpression(member),
        )),
        ast::Expression::PrivateFieldExpression(member) => Some(ast::SimpleAssignmentTarget::from(
            ast::MemberExpression::PrivateFieldExpression(member),
        )),
        _ => None,
    }
}

fn expression_to_assignment_target<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::AssignmentTarget<'a>> {
    Some(ast::AssignmentTarget::from(
        expression_to_simple_assignment_target(builder, expression)?,
    ))
}

fn is_binding_identifier_named(pattern: &ast::BindingPattern<'_>, name: &str) -> bool {
    matches!(
        pattern,
        ast::BindingPattern::BindingIdentifier(identifier) if identifier.name == name
    )
}

fn same_place(left: &Place, right: &Place) -> bool {
    left.identifier.id == right.identifier.id
}

fn place_name(place: &Place) -> Option<&str> {
    let name = match place.identifier.name.as_ref()? {
        types::IdentifierName::Named(name) | types::IdentifierName::Promoted(name) => name,
    };
    is_identifier_name(name).then_some(name)
}

fn lowered_place_name<'a>(
    place: &'a Place,
    synthetic_param_names: &'a HashMap<IdentifierId, String>,
) -> Option<&'a str> {
    place_name(place).or_else(|| {
        synthetic_param_names
            .get(&place.identifier.id)
            .map(String::as_str)
    })
}

fn synthetic_param_names(hir_function: &HIRFunction) -> HashMap<IdentifierId, String> {
    let mut names = HashMap::new();
    let mut next_temp = 0usize;
    for param in &hir_function.params {
        let identifier = match param {
            types::Argument::Place(place) | types::Argument::Spread(place) => &place.identifier,
        };
        if !identifier
            .name
            .as_ref()
            .is_some_and(|name| is_identifier_name(name.value()))
        {
            names.insert(identifier.id, format!("t{next_temp}"));
            next_temp += 1;
        }
    }
    for (_, block) in &hir_function.body.blocks {
        if let Terminal::Try {
            handler_binding, ..
        } = &block.terminal
            && let Some(binding) = handler_binding
            && !binding
                .identifier
                .name
                .as_ref()
                .is_some_and(|name| is_identifier_name(name.value()))
            && !names.contains_key(&binding.identifier.id)
        {
            names.insert(binding.identifier.id, format!("t{next_temp}"));
            next_temp += 1;
        }
    }
    names
}

fn variable_declaration_kind(kind: InstructionKind) -> Option<ast::VariableDeclarationKind> {
    match kind {
        InstructionKind::Const | InstructionKind::HoistedConst => {
            Some(ast::VariableDeclarationKind::Const)
        }
        InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Catch => {
            Some(ast::VariableDeclarationKind::Let)
        }
        InstructionKind::Reassign
        | InstructionKind::Function
        | InstructionKind::HoistedFunction => None,
    }
}

fn collect_same_block_suppressed_declare_instruction_ids(
    instructions: &[Instruction],
) -> HashSet<types::InstructionId> {
    let mut materialized = HashSet::new();
    let mut suppressed = HashSet::new();
    for instruction in instructions.iter().rev() {
        match &instruction.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } => {
                if lvalue.place.identifier.name.is_some()
                    && variable_declaration_kind(lvalue.kind).is_some()
                {
                    materialized.insert(lvalue.place.identifier.declaration_id);
                }
            }
            InstructionValue::DeclareLocal { lvalue, .. }
            | InstructionValue::DeclareContext { lvalue, .. } => {
                if materialized.contains(&lvalue.place.identifier.declaration_id) {
                    suppressed.insert(instruction.id);
                } else {
                    materialized.insert(lvalue.place.identifier.declaration_id);
                }
            }
            _ => {}
        }
    }
    suppressed
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use oxc_allocator::Allocator;
    use oxc_ast::AstBuilder;
    use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
    use oxc_span::{SPAN, SourceType};

    use crate::{
        environment::Environment,
        hir::types::{
            self, BasicBlock, BlockId, DeclarationId, Effect, HIR, HIRFunction, Identifier,
            IdentifierId, IdentifierName, Instruction, InstructionId, InstructionKind,
            MutableRange, Place, ReactFunctionType, SourceLocation, Terminal, Type,
            make_temporary_identifier,
        },
        options::EnvironmentConfig,
    };

    use super::{try_lower_function_body_ast, try_lower_function_declaration_ast};

    fn try_lower_function_body(hir_function: &HIRFunction) -> Option<String> {
        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let body = try_lower_function_body_ast(builder, hir_function)?;
        let program = builder.program(
            SPAN,
            SourceType::mjs(),
            "",
            builder.vec(),
            None,
            builder.vec(),
            body,
        );
        let options = CodegenOptions {
            indent_char: IndentChar::Space,
            indent_width: 2,
            ..CodegenOptions::default()
        };
        Some(Codegen::new().with_options(options).build(&program).code)
    }

    #[test]
    fn lowers_straight_line_temp_return() {
        let temp_left = temporary_place(0);
        let temp_right = temporary_place(1);
        let temp_sum = temporary_place(2);

        let hir = hir_function(
            vec![
                instruction(
                    0,
                    temp_left.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    1,
                    temp_right.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(2.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    2,
                    temp_sum.clone(),
                    types::InstructionValue::BinaryExpression {
                        operator: types::BinaryOperator::Add,
                        left: temp_left,
                        right: temp_right,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: temp_sum.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(3),
            },
            temp_sum,
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return 1 + 2;\n")
        );
    }

    #[test]
    fn elides_trailing_undefined_return() {
        let temp_undefined = temporary_place(4);

        let hir = hir_function(
            vec![instruction(
                4,
                temp_undefined.clone(),
                types::InstructionValue::Primitive {
                    value: types::PrimitiveValue::Undefined,
                    loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: temp_undefined.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(5),
            },
            temp_undefined,
        );

        assert_eq!(try_lower_function_body(&hir).as_deref(), Some(""));
    }

    #[test]
    fn lowers_named_inputs_inside_temp_call() {
        let foo = named_place(10, 10, "foo");
        let temp_arg = temporary_place(11);
        let temp_call = temporary_place(12);

        let hir = hir_function(
            vec![
                instruction(
                    11,
                    temp_arg.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::String("x".to_string()),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    12,
                    temp_call.clone(),
                    types::InstructionValue::CallExpression {
                        callee: foo,
                        args: vec![types::Argument::Place(temp_arg)],
                        optional: false,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: temp_call.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(13),
            },
            temp_call,
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return foo(\"x\");\n")
        );
    }

    #[test]
    fn rejects_named_intermediate_bindings() {
        let named = named_place(20, 20, "value");
        let hir = hir_function(
            vec![instruction(
                20,
                named.clone(),
                types::InstructionValue::Primitive {
                    value: types::PrimitiveValue::Number(1.0),
                    loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: named.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(21),
            },
            named,
        );

        assert_eq!(try_lower_function_body(&hir), None);
    }

    #[test]
    fn lowers_named_store_statements() {
        let temp = temporary_place(30);
        let named = named_place(31, 31, "value");

        let hir = hir_function(
            vec![
                instruction(
                    30,
                    temp.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    31,
                    temporary_place(32),
                    types::InstructionValue::StoreLocal {
                        lvalue: types::LValue {
                            place: named.clone(),
                            kind: InstructionKind::Const,
                        },
                        value: temp,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: named,
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(33),
            },
            temporary_place(34),
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("const value = 1;\nreturn value;\n")
        );
    }

    #[test]
    fn lowers_array_and_object_literals() {
        let one = temporary_place(40);
        let spread = named_place(41, 41, "rest");
        let array = temporary_place(42);
        let object = temporary_place(43);

        let hir = hir_function(
            vec![
                instruction(
                    40,
                    one.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    42,
                    array.clone(),
                    types::InstructionValue::ArrayExpression {
                        elements: vec![
                            types::ArrayElement::Place(one.clone()),
                            types::ArrayElement::Hole,
                            types::ArrayElement::Spread(spread.clone()),
                        ],
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    43,
                    object.clone(),
                    types::InstructionValue::ObjectExpression {
                        properties: vec![
                            types::ObjectPropertyOrSpread::Property(types::ObjectProperty {
                                key: types::ObjectPropertyKey::Identifier("array".to_string()),
                                type_: types::ObjectPropertyType::Property,
                                place: array.clone(),
                            }),
                            types::ObjectPropertyOrSpread::Spread(spread),
                        ],
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: object,
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(44),
            },
            temporary_place(45),
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return {\n  array: [\n    1,\n    ,\n    ...rest\n  ],\n  ...rest\n};\n")
        );
    }

    #[test]
    fn lowers_if_with_fallthrough() {
        let test = named_place(50, 50, "flag");
        let consequent_value = temporary_place(51);
        let alternate_value = temporary_place(52);
        let value_place = named_place(53, 53, "value");

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                61,
                                temporary_place(62),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::If {
                                test,
                                loc: SourceLocation::Generated,
                                consequent: BlockId(1),
                                alternate: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(50),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![
                                instruction(
                                    51,
                                    consequent_value.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(1.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    53,
                                    temporary_place(56),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: consequent_value,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                loc: SourceLocation::Generated,
                                block: BlockId(3),
                                variant: types::GotoVariant::Break,
                                id: InstructionId(57),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    52,
                                    alternate_value.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(2.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    54,
                                    temporary_place(58),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: alternate_value,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                loc: SourceLocation::Generated,
                                block: BlockId(3),
                                variant: types::GotoVariant::Break,
                                id: InstructionId(59),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(60),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some(
                "let value;\nif (flag) {\n  value = 1;\n} else {\n  value = 2;\n}\nreturn value;\n"
            )
        );
    }

    #[test]
    fn lowers_simple_while_loop() {
        let flag = named_place(70, 70, "flag");
        let value_place = named_place(71, 71, "value");
        let test_temp = temporary_place(72);
        let body_value = temporary_place(73);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                74,
                                temporary_place(75),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::While {
                                loc: SourceLocation::Generated,
                                test: BlockId(1),
                                loop_block: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(76),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![instruction(
                                77,
                                test_temp.clone(),
                                types::InstructionValue::LoadLocal {
                                    place: flag,
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(2),
                                variant: types::GotoVariant::Continue,
                                loc: SourceLocation::Generated,
                                id: InstructionId(78),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    79,
                                    body_value.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(1.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    80,
                                    temporary_place(81),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: body_value,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(1),
                                variant: types::GotoVariant::Continue,
                                loc: SourceLocation::Generated,
                                id: InstructionId(82),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(83),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("let value;\nwhile (flag) {\n  value = 1;\n}\nreturn value;\n")
        );
    }

    #[test]
    fn lowers_basic_for_loop() {
        let i_place = named_place(90, 90, "i");
        let sum_place = named_place(91, 91, "sum");
        let zero = temporary_place(92);
        let one = temporary_place(93);
        let three = temporary_place(94);
        let test_expr = temporary_place(95);
        let add_expr = temporary_place(96);
        let inc_expr = temporary_place(97);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: sum_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                98,
                                temporary_place(99),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: sum_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::For {
                                loc: SourceLocation::Generated,
                                init: BlockId(1),
                                test: BlockId(2),
                                update: Some(BlockId(3)),
                                loop_block: BlockId(4),
                                fallthrough: BlockId(5),
                                id: InstructionId(100),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId(1),
                            instructions: vec![
                                instruction(
                                    101,
                                    zero.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(0.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    102,
                                    temporary_place(103),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: i_place.clone(),
                                            kind: InstructionKind::Let,
                                        },
                                        value: zero.clone(),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(2),
                                variant: types::GotoVariant::Break,
                                loc: SourceLocation::Generated,
                                id: InstructionId(104),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Value,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    105,
                                    three.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(3.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    106,
                                    test_expr.clone(),
                                    types::InstructionValue::BinaryExpression {
                                        operator: types::BinaryOperator::Lt,
                                        left: i_place.clone(),
                                        right: three,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Branch {
                                test: test_expr,
                                loc: SourceLocation::Generated,
                                consequent: BlockId(4),
                                alternate: BlockId(5),
                                fallthrough: BlockId(5),
                                id: InstructionId(107),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId(3),
                            instructions: vec![
                                instruction(
                                    108,
                                    one.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(1.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    109,
                                    inc_expr.clone(),
                                    types::InstructionValue::BinaryExpression {
                                        operator: types::BinaryOperator::Add,
                                        left: i_place.clone(),
                                        right: one,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    110,
                                    temporary_place(111),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: i_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: inc_expr,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(2),
                                variant: types::GotoVariant::Break,
                                loc: SourceLocation::Generated,
                                id: InstructionId(112),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(4),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(4),
                            instructions: vec![
                                instruction(
                                    113,
                                    add_expr.clone(),
                                    types::InstructionValue::BinaryExpression {
                                        operator: types::BinaryOperator::Add,
                                        left: sum_place.clone(),
                                        right: i_place.clone(),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    114,
                                    temporary_place(115),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: sum_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: add_expr,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: types::GotoVariant::Continue,
                                loc: SourceLocation::Generated,
                                id: InstructionId(116),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(5),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(5),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: sum_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(117),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some(
                "let sum;\nfor (let i = 0; i < 3; i = i + 1) {\n  sum = sum + i;\n}\nreturn sum;\n"
            )
        );
    }

    #[test]
    fn lowers_try_catch() {
        let value_place = named_place(120, 120, "value");
        let catch_place = named_place(121, 121, "err");
        let one = temporary_place(122);
        let two = temporary_place(123);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                124,
                                temporary_place(125),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Try {
                                block: BlockId(1),
                                handler_binding: Some(catch_place),
                                loc: SourceLocation::Generated,
                                handler: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(126),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![instruction(
                                127,
                                one.clone(),
                                types::InstructionValue::Primitive {
                                    value: types::PrimitiveValue::Number(1.0),
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Throw {
                                value: one,
                                loc: SourceLocation::Generated,
                                id: InstructionId(128),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Catch,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    129,
                                    two.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(2.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    130,
                                    temporary_place(131),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: two,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                loc: SourceLocation::Generated,
                                block: BlockId(3),
                                variant: types::GotoVariant::Break,
                                id: InstructionId(132),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(133),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some(
                "let value;\ntry {\n  throw 1;\n} catch (err) {\n  value = 2;\n}\nreturn value;\n"
            )
        );
    }

    #[test]
    fn falls_back_on_switch_with_breaks() {
        let tag_place = named_place(140, 140, "tag");
        let value_place = named_place(141, 141, "value");
        let one = temporary_place(142);
        let two = temporary_place(143);
        let three = temporary_place(144);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![
                                instruction(
                                    145,
                                    temporary_place(146),
                                    types::InstructionValue::DeclareLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Let,
                                        },
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    147,
                                    one.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(1.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Switch {
                                test: tag_place,
                                cases: vec![
                                    types::SwitchCase {
                                        test: Some(one),
                                        block: BlockId(1),
                                    },
                                    types::SwitchCase {
                                        test: None,
                                        block: BlockId(2),
                                    },
                                ],
                                loc: SourceLocation::Generated,
                                fallthrough: BlockId(3),
                                id: InstructionId(148),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![
                                instruction(
                                    149,
                                    two.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(2.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    150,
                                    temporary_place(151),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: two,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: types::GotoVariant::Break,
                                loc: SourceLocation::Generated,
                                id: InstructionId(152),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    153,
                                    three.clone(),
                                    types::InstructionValue::Primitive {
                                        value: types::PrimitiveValue::Number(3.0),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    154,
                                    temporary_place(155),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: value_place.clone(),
                                            kind: InstructionKind::Reassign,
                                        },
                                        value: three,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: types::GotoVariant::Break,
                                loc: SourceLocation::Generated,
                                id: InstructionId(156),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(157),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(try_lower_function_body(&hir), None);
    }

    #[test]
    fn lowers_logical_and_ternary_expressions() {
        let flag_place = named_place(160, 160, "flag");
        let other_place = named_place(161, 161, "other");
        let one = temporary_place(162);
        let two = temporary_place(163);
        let logical = temporary_place(164);
        let ternary = temporary_place(165);

        let hir = hir_function(
            vec![
                instruction(
                    166,
                    one.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    167,
                    two.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(2.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    168,
                    logical.clone(),
                    types::InstructionValue::LogicalExpression {
                        operator: types::LogicalOperator::And,
                        left: flag_place,
                        right: other_place,
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    169,
                    ternary.clone(),
                    types::InstructionValue::Ternary {
                        test: logical,
                        consequent: one,
                        alternate: two,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: ternary,
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(170),
            },
            temporary_place(171),
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return flag && other ? 1 : 2;\n")
        );
    }

    #[test]
    fn lowers_side_effecting_mutation_statements() {
        let index_place = named_place(180, 180, "i");
        let object_place = named_place(181, 181, "obj");
        let key_place = named_place(182, 182, "key");

        let hir = hir_function(
            vec![
                instruction(
                    183,
                    temporary_place(184),
                    types::InstructionValue::PrefixUpdate {
                        lvalue: index_place.clone(),
                        operation: types::UpdateOperator::Increment,
                        value: index_place.clone(),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    185,
                    temporary_place(186),
                    types::InstructionValue::ComputedStore {
                        object: object_place.clone(),
                        property: key_place,
                        value: index_place.clone(),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    187,
                    temporary_place(188),
                    types::InstructionValue::PropertyDelete {
                        object: object_place,
                        property: types::PropertyLiteral::String("value".to_string()),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    189,
                    temporary_place(190),
                    types::InstructionValue::Debugger {
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: index_place,
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(191),
            },
            temporary_place(192),
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("++i;\nobj[key] = i;\ndelete obj.value;\ndebugger;\nreturn i;\n")
        );
    }

    #[test]
    fn lowers_basic_for_of_loop() {
        let items_place = named_place(200, 200, "items");
        let item_place = named_place(201, 201, "item");
        let value_place = named_place(202, 202, "value");
        let iterator_place = temporary_place(203);
        let next_place = temporary_place(204);
        let test_place = temporary_place(205);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                206,
                                temporary_place(207),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::ForOf {
                                loc: SourceLocation::Generated,
                                init: BlockId(1),
                                test: BlockId(2),
                                loop_block: BlockId(3),
                                fallthrough: BlockId(4),
                                id: InstructionId(208),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId(1),
                            instructions: vec![instruction(
                                209,
                                iterator_place.clone(),
                                types::InstructionValue::GetIterator {
                                    collection: items_place.clone(),
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(2),
                                variant: types::GotoVariant::Break,
                                loc: SourceLocation::Generated,
                                id: InstructionId(210),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId(2),
                            instructions: vec![
                                instruction(
                                    211,
                                    next_place.clone(),
                                    types::InstructionValue::IteratorNext {
                                        iterator: iterator_place,
                                        collection: items_place,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    212,
                                    temporary_place(213),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: item_place.clone(),
                                            kind: InstructionKind::Let,
                                        },
                                        value: next_place,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    214,
                                    test_place.clone(),
                                    types::InstructionValue::LoadLocal {
                                        place: item_place.clone(),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Branch {
                                test: test_place,
                                loc: SourceLocation::Generated,
                                consequent: BlockId(3),
                                alternate: BlockId(4),
                                fallthrough: BlockId(4),
                                id: InstructionId(215),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![instruction(
                                216,
                                temporary_place(217),
                                types::InstructionValue::StoreLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Reassign,
                                    },
                                    value: item_place,
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(1),
                                variant: types::GotoVariant::Continue,
                                loc: SourceLocation::Generated,
                                id: InstructionId(218),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(4),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(4),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(219),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("let value;\nfor (let item of items) {\n  value = item;\n}\nreturn value;\n")
        );
    }

    #[test]
    fn lowers_basic_for_in_loop() {
        let object_place = named_place(220, 220, "obj");
        let key_place = named_place(221, 221, "key");
        let value_place = named_place(222, 222, "value");
        let next_place = temporary_place(223);
        let test_place = temporary_place(224);

        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![instruction(
                                225,
                                temporary_place(226),
                                types::InstructionValue::DeclareLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Let,
                                    },
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::ForIn {
                                loc: SourceLocation::Generated,
                                init: BlockId(1),
                                loop_block: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(227),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId(1),
                            instructions: vec![
                                instruction(
                                    228,
                                    next_place.clone(),
                                    types::InstructionValue::NextPropertyOf {
                                        value: object_place.clone(),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    229,
                                    temporary_place(230),
                                    types::InstructionValue::StoreLocal {
                                        lvalue: types::LValue {
                                            place: key_place.clone(),
                                            kind: InstructionKind::Let,
                                        },
                                        value: next_place,
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                                instruction(
                                    231,
                                    test_place.clone(),
                                    types::InstructionValue::LoadLocal {
                                        place: key_place.clone(),
                                        loc: SourceLocation::Generated,
                                    },
                                ),
                            ],
                            terminal: Terminal::Branch {
                                test: test_place,
                                loc: SourceLocation::Generated,
                                consequent: BlockId(2),
                                alternate: BlockId(3),
                                fallthrough: BlockId(3),
                                id: InstructionId(232),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(2),
                            instructions: vec![instruction(
                                233,
                                temporary_place(234),
                                types::InstructionValue::StoreLocal {
                                    lvalue: types::LValue {
                                        place: value_place.clone(),
                                        kind: InstructionKind::Reassign,
                                    },
                                    value: key_place,
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId(1),
                                variant: types::GotoVariant::Continue,
                                loc: SourceLocation::Generated,
                                id: InstructionId(235),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(236),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("let value;\nfor (let key in obj) {\n  value = key;\n}\nreturn value;\n")
        );
    }

    #[test]
    fn lowers_function_declaration_with_named_params() {
        let arg = named_place(230, 230, "arg");
        let rest = named_place(231, 231, "rest");

        let mut hir = hir_function(
            vec![],
            Terminal::Return {
                value: arg.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(232),
            },
            arg,
        );
        hir.id = Some("outlined".to_string());
        hir.params = vec![types::Argument::Place(named_place(233, 233, "arg"))];
        hir.params.push(types::Argument::Spread(rest));

        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let statement =
            try_lower_function_declaration_ast(builder, &hir).expect("should lower declaration");
        let program = builder.program(
            SPAN,
            SourceType::mjs(),
            "",
            builder.vec(),
            None,
            builder.vec(),
            builder.vec1(statement),
        );
        let code = Codegen::new()
            .with_options(CodegenOptions {
                indent_char: IndentChar::Space,
                indent_width: 2,
                ..CodegenOptions::default()
            })
            .build(&program)
            .code;

        assert_eq!(
            code,
            "function outlined(arg, ...rest) {\n  return arg;\n}\n"
        );
    }

    #[test]
    fn lowers_function_declaration_with_directives() {
        let value = named_place(234, 234, "value");

        let mut hir = hir_function(
            vec![],
            Terminal::Return {
                value: value.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(235),
            },
            value,
        );
        hir.id = Some("outlined".to_string());
        hir.directives = vec!["worklet".to_string()];

        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let statement =
            try_lower_function_declaration_ast(builder, &hir).expect("should lower declaration");
        let program = builder.program(
            SPAN,
            SourceType::mjs(),
            "",
            builder.vec(),
            None,
            builder.vec(),
            builder.vec1(statement),
        );
        let code = Codegen::new()
            .with_options(CodegenOptions {
                indent_char: IndentChar::Space,
                indent_width: 2,
                ..CodegenOptions::default()
            })
            .build(&program)
            .code;

        assert_eq!(
            code,
            "function outlined() {\n  \"worklet\";\n  return value;\n}\n"
        );
    }

    #[test]
    fn lowers_function_declaration_with_unnamed_params() {
        let temp = temporary_place(240);
        let rest = temporary_place(242);

        let mut hir = hir_function(
            vec![],
            Terminal::Return {
                value: temp.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(241),
            },
            temp.clone(),
        );
        hir.id = Some("outlined".to_string());
        hir.params = vec![
            types::Argument::Place(temp.clone()),
            types::Argument::Spread(rest),
        ];

        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let statement =
            try_lower_function_declaration_ast(builder, &hir).expect("should lower declaration");
        let program = builder.program(
            SPAN,
            SourceType::mjs(),
            "",
            builder.vec(),
            None,
            builder.vec(),
            builder.vec1(statement),
        );
        let code = Codegen::new()
            .with_options(CodegenOptions {
                indent_char: IndentChar::Space,
                indent_width: 2,
                ..CodegenOptions::default()
            })
            .build(&program)
            .code;

        assert_eq!(code, "function outlined(t0, ...t1) {\n  return t0;\n}\n");
    }

    #[test]
    fn lowers_function_declaration_returning_jsx() {
        let component = named_place(250, 250, "Item");
        let item = named_place(251, 251, "item");
        let jsx = temporary_place(252);

        let mut hir = hir_function(
            vec![instruction(
                252,
                jsx.clone(),
                types::InstructionValue::JsxExpression {
                    tag: types::JsxTag::Component(component),
                    props: vec![types::JsxAttribute::Attribute {
                        name: "item".to_string(),
                        place: item.clone(),
                    }],
                    children: None,
                    loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: jsx.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(253),
            },
            jsx,
        );
        hir.id = Some("outlined".to_string());
        hir.params = vec![types::Argument::Place(item)];

        let allocator = Allocator::default();
        let builder = AstBuilder::new(&allocator);
        let statement =
            try_lower_function_declaration_ast(builder, &hir).expect("should lower declaration");
        let program = builder.program(
            SPAN,
            SourceType::mjs().with_jsx(true),
            "",
            builder.vec(),
            None,
            builder.vec(),
            builder.vec1(statement),
        );
        let code = Codegen::new()
            .with_options(CodegenOptions {
                indent_char: IndentChar::Space,
                indent_width: 2,
                ..CodegenOptions::default()
            })
            .build(&program)
            .code;

        assert_eq!(
            code,
            "function outlined(item) {\n  return <Item item={item} />;\n}\n"
        );
    }

    #[test]
    fn lowers_nested_arrow_function_expression() {
        let captured = named_place(260, 260, "x");
        let inner_value = temporary_place(261);
        let inner_hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: captured.clone(),
            context: vec![captured.clone()],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId(0),
                        instructions: vec![],
                        terminal: Terminal::Return {
                            value: captured.clone(),
                            return_variant: types::ReturnVariant::Explicit,
                            loc: SourceLocation::Generated,
                            id: InstructionId(262),
                        },
                        preds: HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };
        let hir = hir_function(
            vec![instruction(
                261,
                inner_value.clone(),
                types::InstructionValue::FunctionExpression {
                    name: None,
                    lowered_func: types::LoweredFunction { func: inner_hir },
                    expr_type: types::FunctionExpressionType::ArrowFunctionExpression,
                    loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: inner_value.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(263),
            },
            inner_value,
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return () => x;\n")
        );
    }

    #[test]
    fn lowers_template_literal_expression() {
        let item = named_place(285, 285, "i");
        let template = temporary_place(286);
        let hir = hir_function(
            vec![instruction(
                287,
                template.clone(),
                types::InstructionValue::TemplateLiteral {
                    subexprs: vec![item.clone()],
                    quasis: vec![
                        types::TemplateQuasi {
                            raw: "button-".to_string(),
                            cooked: Some("button-".to_string()),
                        },
                        types::TemplateQuasi {
                            raw: String::new(),
                            cooked: Some(String::new()),
                        },
                    ],
                    loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: template.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(288),
            },
            template,
        );
        let mut hir = hir;
        hir.params = vec![types::Argument::Place(item)];

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("return `button-${i}`;\n")
        );
    }

    #[test]
    fn lowers_try_catch_with_synthetic_catch_binding() {
        let array_value = temporary_place(290);
        let undefined_value = temporary_place(291);
        let catch_binding = Place {
            identifier: Identifier {
                id: IdentifierId(292),
                declaration_id: DeclarationId(292),
                name: Some(IdentifierName::Promoted("#t3".to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        };
        let hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: undefined_value.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![],
                            terminal: Terminal::Try {
                                block: BlockId(1),
                                handler_binding: Some(catch_binding),
                                loc: SourceLocation::Generated,
                                handler: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(293),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(1),
                            instructions: vec![instruction(
                                294,
                                array_value.clone(),
                                types::InstructionValue::ArrayExpression {
                                    elements: vec![],
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Return {
                                value: array_value,
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(295),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(2),
                        BasicBlock {
                            kind: types::BlockKind::Catch,
                            id: BlockId(2),
                            instructions: vec![instruction(
                                296,
                                undefined_value.clone(),
                                types::InstructionValue::Primitive {
                                    value: types::PrimitiveValue::Undefined,
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Return {
                                value: undefined_value.clone(),
                                return_variant: types::ReturnVariant::Explicit,
                                loc: SourceLocation::Generated,
                                id: InstructionId(297),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![],
                            terminal: Terminal::Unreachable {
                                loc: SourceLocation::Generated,
                                id: InstructionId(299),
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("try {\n  return [];\n} catch (t0) {\n  return;\n}\n")
        );
    }

    #[test]
    fn suppresses_same_block_declare_before_materialized_const_store() {
        let x = named_place(270, 270, "x");
        let inner = named_place(271, 271, "inner");
        let inner_value = temporary_place(272);
        let three_value = temporary_place(273);
        let call_value = temporary_place(274);
        let inner_hir = HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: x.clone(),
            context: vec![x.clone()],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId(0),
                        instructions: vec![],
                        terminal: Terminal::Return {
                            value: x.clone(),
                            return_variant: types::ReturnVariant::Explicit,
                            loc: SourceLocation::Generated,
                            id: InstructionId(275),
                        },
                        preds: HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };
        let hir = hir_function(
            vec![
                instruction(
                    276,
                    temporary_place(276),
                    types::InstructionValue::DeclareLocal {
                        lvalue: types::LValue {
                            place: x.clone(),
                            kind: InstructionKind::Let,
                        },
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    277,
                    inner_value.clone(),
                    types::InstructionValue::FunctionExpression {
                        name: None,
                        lowered_func: types::LoweredFunction { func: inner_hir },
                        expr_type: types::FunctionExpressionType::ArrowFunctionExpression,
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    278,
                    temporary_place(278),
                    types::InstructionValue::StoreLocal {
                        lvalue: types::LValue {
                            place: inner.clone(),
                            kind: InstructionKind::Const,
                        },
                        value: inner_value,
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    279,
                    three_value.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Number(3.0),
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    280,
                    temporary_place(280),
                    types::InstructionValue::StoreLocal {
                        lvalue: types::LValue {
                            place: x.clone(),
                            kind: InstructionKind::Const,
                        },
                        value: three_value,
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    281,
                    call_value.clone(),
                    types::InstructionValue::CallExpression {
                        callee: inner,
                        args: vec![],
                        optional: false,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: call_value.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(282),
            },
            call_value,
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("const inner = () => x;\nconst x = 3;\nreturn inner();\n")
        );
    }

    #[test]
    fn suppresses_earlier_duplicate_context_declare() {
        let interval_id = named_place(320, 320, "intervalId");
        let undefined_value = temporary_place(321);
        let hir = hir_function(
            vec![
                instruction(
                    322,
                    temporary_place(322),
                    types::InstructionValue::DeclareContext {
                        lvalue: types::LValue {
                            place: interval_id.clone(),
                            kind: InstructionKind::HoistedLet,
                        },
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    323,
                    temporary_place(323),
                    types::InstructionValue::DeclareContext {
                        lvalue: types::LValue {
                            place: interval_id,
                            kind: InstructionKind::Let,
                        },
                        loc: SourceLocation::Generated,
                    },
                ),
                instruction(
                    324,
                    undefined_value.clone(),
                    types::InstructionValue::Primitive {
                        value: types::PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    },
                ),
            ],
            Terminal::Return {
                value: undefined_value.clone(),
                return_variant: types::ReturnVariant::Explicit,
                loc: SourceLocation::Generated,
                id: InstructionId(325),
            },
            undefined_value,
        );

        assert_eq!(
            try_lower_function_body(&hir).as_deref(),
            Some("let intervalId;\n")
        );
    }

    fn hir_function(
        instructions: Vec<Instruction>,
        terminal: Terminal,
        returns: Place,
    ) -> HIRFunction {
        HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns,
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
                    BlockId(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId(0),
                        instructions,
                        terminal,
                        preds: HashSet::new(),
                        phis: vec![],
                    },
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn instruction(id: u32, lvalue: Place, value: types::InstructionValue) -> Instruction {
        Instruction {
            id: InstructionId(id),
            lvalue,
            value,
            loc: SourceLocation::Generated,
            effects: None,
        }
    }

    fn temporary_place(id: u32) -> Place {
        Place {
            identifier: make_temporary_identifier(IdentifierId(id), SourceLocation::Generated),
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn named_place(id: u32, declaration_id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(declaration_id),
                name: Some(IdentifierName::Named(name.to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }
}
