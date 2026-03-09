use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_span::{SPAN, SourceType};
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
            ) || is_self_referential_property_store(instruction, &instruction_map)
                || is_assignment_value_sensitive_store(instruction, &used_temps)
        })
    }) {
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

pub(crate) fn try_lower_function_body(hir_function: &HIRFunction) -> Option<String> {
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

        let mut statements = self.builder.vec();
        for instruction in &block.instructions {
            if let Some(statement) = self.lower_instruction_to_statement(instruction)? {
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

    fn lower_instruction_to_statement(
        &self,
        instruction: &Instruction,
    ) -> Option<Option<ast::Statement<'a>>> {
        match &instruction.value {
            InstructionValue::DeclareLocal { lvalue, .. }
            | InstructionValue::DeclareContext { lvalue, .. } => {
                if lvalue.kind == InstructionKind::Catch {
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
        let name = place_name(place)?;
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
                self.lower_place(right, visiting)?,
            )),
            InstructionValue::Ternary {
                test,
                consequent,
                alternate,
                ..
            } => Some(self.builder.expression_conditional(
                SPAN,
                self.lower_place(test, visiting)?,
                self.lower_place(consequent, visiting)?,
                self.lower_place(alternate, visiting)?,
            )),
            InstructionValue::CallExpression {
                callee,
                args,
                optional,
                ..
            } => Some(self.builder.expression_call(
                SPAN,
                self.lower_place(callee, visiting)?,
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
                    callee,
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
                visiting,
            ),
            InstructionValue::JsxFragment { children, .. } => lower_jsx_fragment_expression(
                self.builder,
                children,
                |place, visiting| self.lower_place(place, visiting),
                visiting,
            ),
            InstructionValue::JSXText { value, .. } => {
                Some(self.builder.expression_string_literal(SPAN, self.builder.atom(value), None))
            }
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
            InstructionValue::MetaProperty { meta, property, .. } => Some(
                self.builder.expression_meta_property(
                    SPAN,
                    self.builder.identifier_name(SPAN, self.builder.ident(meta)),
                    self.builder
                        .identifier_name(SPAN, self.builder.ident(property)),
                ),
            ),
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

        Some(ast::Expression::from(
            self.builder.member_expression_computed(
                SPAN,
                self.lower_place(receiver, visiting)?,
                self.lower_place(property, visiting)?,
                receiver_optional,
            ),
        ))
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
        if statements.is_empty() {
            return Some(None);
        }
        let expressions = statements
            .into_iter()
            .map(|statement| statement_to_expression(statement, self.builder.allocator))
            .collect::<Option<Vec<_>>>()?;
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

fn lower_property_load<'a, F>(
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

fn lower_property_assignment_target<'a, F>(
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

fn lower_jsx_expression<'a, F>(
    builder: AstBuilder<'a>,
    tag: &types::JsxTag,
    props: &[types::JsxAttribute],
    children: Option<&[Place]>,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    if matches!(tag, types::JsxTag::Fragment) {
        return lower_jsx_fragment_expression(
            builder,
            children.unwrap_or(&[]),
            lower_place,
            visiting,
        );
    }

    let opening_name = lower_jsx_element_name(builder, tag, lower_place, visiting)?;
    let attributes = lower_jsx_attributes(builder, props, lower_place, visiting)?;
    let jsx_children =
        lower_jsx_children(builder, children.unwrap_or(&[]), lower_place, visiting)?;
    let closing_element = if jsx_children.is_empty() {
        None
    } else {
        Some(builder.alloc_jsx_closing_element(
            SPAN,
            lower_jsx_element_name(builder, tag, lower_place, visiting)?,
        ))
    };

    Some(builder.expression_jsx_element(
        SPAN,
        builder.alloc_jsx_opening_element(SPAN, opening_name, NONE, attributes),
        jsx_children,
        closing_element,
    ))
}

fn lower_jsx_fragment_expression<'a, F>(
    builder: AstBuilder<'a>,
    children: &[Place],
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    Some(builder.expression_jsx_fragment(
        SPAN,
        builder.jsx_opening_fragment(SPAN),
        lower_jsx_children(builder, children, lower_place, visiting)?,
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
        types::JsxTag::Fragment => None,
    }
}

fn expression_to_jsx_element_name<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::JSXElementName<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => Some(
            builder.jsx_element_name_identifier_reference(SPAN, identifier.name),
        ),
        ast::Expression::StaticMemberExpression(member) => Some(
            builder.jsx_element_name_member_expression(
                SPAN,
                expression_to_jsx_member_expression_object(
                    builder,
                    member.object.clone_in(builder.allocator),
                )?,
                builder.jsx_identifier(SPAN, member.property.name),
            ),
        ),
        ast::Expression::ThisExpression(_) => Some(builder.jsx_element_name_this_expression(SPAN)),
        _ => None,
    }
}

fn expression_to_jsx_member_expression_object<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::JSXMemberExpressionObject<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => Some(
            builder.jsx_member_expression_object_identifier_reference(SPAN, identifier.name),
        ),
        ast::Expression::StaticMemberExpression(member) => Some(
            builder.jsx_member_expression_object_member_expression(
                SPAN,
                expression_to_jsx_member_expression_object(
                    builder,
                    member.object.clone_in(builder.allocator),
                )?,
                builder.jsx_identifier(SPAN, member.property.name),
            ),
        ),
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
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::JSXAttributeItem<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let mut attributes = builder.vec();
    for prop in props {
        match prop {
            types::JsxAttribute::Attribute { name, place } => {
                let value = lower_jsx_attribute_value(builder, place, lower_place, visiting)?;
                attributes.push(builder.jsx_attribute_item_attribute(
                    SPAN,
                    builder.jsx_attribute_name_identifier(SPAN, builder.atom(name)),
                    value,
                ));
            }
            types::JsxAttribute::SpreadAttribute { argument } => {
                attributes.push(builder.jsx_attribute_item_spread_attribute(
                    SPAN,
                    lower_place(argument, visiting)?,
                ));
            }
        }
    }
    Some(attributes)
}

fn lower_jsx_attribute_value<'a, F>(
    builder: AstBuilder<'a>,
    place: &Place,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<Option<ast::JSXAttributeValue<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let expression = lower_place(place, visiting)?;
    match expression {
        ast::Expression::BooleanLiteral(boolean) if boolean.value => Some(None),
        ast::Expression::StringLiteral(literal) => {
            Some(Some(ast::JSXAttributeValue::StringLiteral(literal)))
        }
        ast::Expression::JSXElement(element) => Some(Some(ast::JSXAttributeValue::Element(element))),
        ast::Expression::JSXFragment(fragment) => {
            Some(Some(ast::JSXAttributeValue::Fragment(fragment)))
        }
        expression => Some(Some(ast::JSXAttributeValue::ExpressionContainer(
            builder.alloc_jsx_expression_container(SPAN, ast::JSXExpression::from(expression)),
        ))),
    }
}

fn lower_jsx_children<'a, F>(
    builder: AstBuilder<'a>,
    children: &[Place],
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::JSXChild<'a>>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let mut lowered = builder.vec();
    for child in children {
        lowered.push(lower_jsx_child(builder, child, lower_place, visiting)?);
    }
    Some(lowered)
}

fn lower_jsx_child<'a, F>(
    builder: AstBuilder<'a>,
    place: &Place,
    lower_place: F,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::JSXChild<'a>>
where
    F: Fn(&Place, &mut HashSet<IdentifierId>) -> Option<ast::Expression<'a>> + Copy,
{
    let expression = lower_place(place, visiting)?;
    match expression {
        ast::Expression::StringLiteral(literal) => Some(builder.jsx_child_expression_container(
            SPAN,
            ast::JSXExpression::StringLiteral(literal),
        )),
        ast::Expression::JSXElement(element) => Some(ast::JSXChild::Element(element)),
        ast::Expression::JSXFragment(fragment) => Some(ast::JSXChild::Fragment(fragment)),
        expression => Some(builder.jsx_child_expression_container(
            SPAN,
            ast::JSXExpression::from(expression),
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

fn lower_primitive<'a>(builder: AstBuilder<'a>, value: &PrimitiveValue) -> ast::Expression<'a> {
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

fn lower_binary_operator(operator: types::BinaryOperator) -> AstBinaryOperator {
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

fn lower_unary_operator(operator: types::UnaryOperator) -> AstUnaryOperator {
    match operator {
        types::UnaryOperator::Minus => AstUnaryOperator::UnaryNegation,
        types::UnaryOperator::Plus => AstUnaryOperator::UnaryPlus,
        types::UnaryOperator::Not => AstUnaryOperator::LogicalNot,
        types::UnaryOperator::BitNot => AstUnaryOperator::BitwiseNot,
        types::UnaryOperator::TypeOf => AstUnaryOperator::Typeof,
        types::UnaryOperator::Void => AstUnaryOperator::Void,
    }
}

fn lower_logical_operator(operator: types::LogicalOperator) -> AstLogicalOperator {
    match operator {
        types::LogicalOperator::And => AstLogicalOperator::And,
        types::LogicalOperator::Or => AstLogicalOperator::Or,
        types::LogicalOperator::NullishCoalescing => AstLogicalOperator::Coalesce,
    }
}

fn lower_update_operator(operator: types::UpdateOperator) -> AstUpdateOperator {
    match operator {
        types::UpdateOperator::Increment => AstUpdateOperator::Increment,
        types::UpdateOperator::Decrement => AstUpdateOperator::Decrement,
    }
}

fn expression_to_simple_assignment_target<'a>(
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
    match place.identifier.name.as_ref()? {
        types::IdentifierName::Named(name) | types::IdentifierName::Promoted(name) => Some(name),
    }
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
        if identifier.name.is_none() {
            names.insert(identifier.id, format!("t{next_temp}"));
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

    use super::{try_lower_function_body, try_lower_function_declaration_ast};

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
                id: InstructionId::new(3),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(5),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(13),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(21),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(33),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(44),
                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                consequent: BlockId::new(1),
                                alternate: BlockId::new(2),
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(50),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(1),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Try,
                                id: InstructionId::new(57),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(2),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Try,
                                id: InstructionId::new(59),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(60),
                                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                test: BlockId::new(1),
                                loop_block: BlockId::new(2),
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(76),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(1),
                            instructions: vec![instruction(
                                77,
                                test_temp.clone(),
                                types::InstructionValue::LoadLocal {
                                    place: flag,
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId::new(2),
                                variant: types::GotoVariant::Continue,
                                id: InstructionId::new(78),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(2),
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
                                block: BlockId::new(1),
                                variant: types::GotoVariant::Continue,
                                id: InstructionId::new(82),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(83),
                                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: sum_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                init: BlockId::new(1),
                                test: BlockId::new(2),
                                update: Some(BlockId::new(3)),
                                loop_block: BlockId::new(4),
                                fallthrough: BlockId::new(5),
                                id: InstructionId::new(100),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId::new(1),
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
                                block: BlockId::new(2),
                                variant: types::GotoVariant::Break,
                                id: InstructionId::new(104),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Value,
                            id: BlockId::new(2),
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
                                consequent: BlockId::new(4),
                                alternate: BlockId::new(5),
                                fallthrough: BlockId::new(5),
                                id: InstructionId::new(107),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId::new(3),
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
                                block: BlockId::new(2),
                                variant: types::GotoVariant::Break,
                                id: InstructionId::new(112),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(4),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(4),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Continue,
                                id: InstructionId::new(116),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(5),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(5),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: sum_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(117),
                                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                block: BlockId::new(1),
                                handler_binding: Some(catch_place),
                                handler: BlockId::new(2),
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(126),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(1),
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
                                id: InstructionId::new(128),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Catch,
                            id: BlockId::new(2),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Try,
                                id: InstructionId::new(132),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(133),
                                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                        block: BlockId::new(1),
                                    },
                                    types::SwitchCase {
                                        test: None,
                                        block: BlockId::new(2),
                                    },
                                ],
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(148),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(1),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Break,
                                id: InstructionId::new(152),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(2),
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
                                block: BlockId::new(3),
                                variant: types::GotoVariant::Break,
                                id: InstructionId::new(156),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(157),
                                loc: SourceLocation::Generated,
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
                id: InstructionId::new(170),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(191),
                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                init: BlockId::new(1),
                                test: BlockId::new(2),
                                loop_block: BlockId::new(3),
                                fallthrough: BlockId::new(4),
                                id: InstructionId::new(208),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId::new(1),
                            instructions: vec![instruction(
                                209,
                                iterator_place.clone(),
                                types::InstructionValue::GetIterator {
                                    collection: items_place.clone(),
                                    loc: SourceLocation::Generated,
                                },
                            )],
                            terminal: Terminal::Goto {
                                block: BlockId::new(2),
                                variant: types::GotoVariant::Break,
                                id: InstructionId::new(210),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId::new(2),
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
                                consequent: BlockId::new(3),
                                alternate: BlockId::new(4),
                                fallthrough: BlockId::new(4),
                                id: InstructionId::new(215),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
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
                                block: BlockId::new(1),
                                variant: types::GotoVariant::Continue,
                                id: InstructionId::new(218),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(4),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(4),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(219),
                                loc: SourceLocation::Generated,
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
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: value_place.clone(),
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![
                    (
                        BlockId::new(0),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(0),
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
                                init: BlockId::new(1),
                                loop_block: BlockId::new(2),
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(227),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(1),
                        BasicBlock {
                            kind: types::BlockKind::Loop,
                            id: BlockId::new(1),
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
                                consequent: BlockId::new(2),
                                alternate: BlockId::new(3),
                                fallthrough: BlockId::new(3),
                                id: InstructionId::new(232),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(2),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(2),
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
                                block: BlockId::new(1),
                                variant: types::GotoVariant::Continue,
                                id: InstructionId::new(235),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId::new(3),
                        BasicBlock {
                            kind: types::BlockKind::Block,
                            id: BlockId::new(3),
                            instructions: vec![],
                            terminal: Terminal::Return {
                                value: value_place,
                                return_variant: types::ReturnVariant::Explicit,
                                id: InstructionId::new(236),
                                loc: SourceLocation::Generated,
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
                id: InstructionId::new(232),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(235),
                loc: SourceLocation::Generated,
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
                id: InstructionId::new(241),
                loc: SourceLocation::Generated,
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
                    opening_loc: SourceLocation::Generated,
                    closing_loc: SourceLocation::Generated,
                },
            )],
            Terminal::Return {
                value: jsx.clone(),
                return_variant: types::ReturnVariant::Explicit,
                id: InstructionId::new(253),
                loc: SourceLocation::Generated,
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

    fn hir_function(
        instructions: Vec<Instruction>,
        terminal: Terminal,
        returns: Place,
    ) -> HIRFunction {
        HIRFunction {
            env: Environment::new(EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns,
            context: vec![],
            body: HIR {
                entry: BlockId::new(0),
                blocks: vec![(
                    BlockId::new(0),
                    BasicBlock {
                        kind: types::BlockKind::Block,
                        id: BlockId::new(0),
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
            id: InstructionId::new(id),
            lvalue,
            value,
            loc: SourceLocation::Generated,
            effects: None,
        }
    }

    fn temporary_place(id: u32) -> Place {
        Place {
            identifier: make_temporary_identifier(IdentifierId::new(id), SourceLocation::Generated),
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn named_place(id: u32, declaration_id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId::new(id),
                declaration_id: DeclarationId::new(declaration_id),
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
