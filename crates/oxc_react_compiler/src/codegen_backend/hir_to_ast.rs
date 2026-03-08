use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_span::{SPAN, SourceType};
use oxc_syntax::{
    identifier::is_identifier_name,
    number::NumberBase,
    operator::{
        AssignmentOperator, BinaryOperator as AstBinaryOperator, UnaryOperator as AstUnaryOperator,
    },
};

use crate::hir::types::{
    self, HIRFunction, IdentifierId, Instruction, InstructionKind, InstructionValue, Place,
    PrimitiveValue, Terminal,
};

pub(crate) fn try_lower_function_body(hir_function: &HIRFunction) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let instruction_map = hir_function
        .body
        .blocks
        .iter()
        .flat_map(|(_, block)| block.instructions.iter())
        .map(|instruction| (instruction.lvalue.identifier.id, instruction))
        .collect::<HashMap<_, _>>();
    let block_map = hir_function
        .body
        .blocks
        .iter()
        .map(|(id, block)| (*id, block))
        .collect::<HashMap<_, _>>();
    let state = LoweringState::new(
        builder,
        &block_map,
        &instruction_map,
        collect_used_temps(hir_function),
    );
    let body = state.lower_block_sequence(
        hir_function.body.entry,
        None,
        &mut HashSet::new(),
        None,
    )?;
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

struct LoweringState<'a, 'hir> {
    builder: AstBuilder<'a>,
    block_map: &'hir HashMap<types::BlockId, &'hir types::BasicBlock>,
    instruction_map: &'hir HashMap<IdentifierId, &'hir Instruction>,
    used_temps: HashSet<IdentifierId>,
}

#[derive(Clone, Copy)]
struct LoopContext {
    continue_target: types::BlockId,
    break_target: types::BlockId,
}

impl<'a, 'hir> LoweringState<'a, 'hir> {
    fn new(
        builder: AstBuilder<'a>,
        block_map: &'hir HashMap<types::BlockId, &'hir types::BasicBlock>,
        instruction_map: &'hir HashMap<IdentifierId, &'hir Instruction>,
        used_temps: HashSet<IdentifierId>,
    ) -> Self {
        Self {
            builder,
            block_map,
            instruction_map,
            used_temps,
        }
    }

    fn lower_terminal(
        &self,
        terminal: &Terminal,
        stop_at: Option<types::BlockId>,
        visiting_blocks: &mut HashSet<types::BlockId>,
        loop_context: Option<LoopContext>,
    ) -> Option<oxc_allocator::Vec<'a, ast::Statement<'a>>> {
        match terminal {
            Terminal::Return { value, .. } => Some(self.builder.vec1(
                self.builder
                    .statement_return(SPAN, Some(self.lower_place(value, &mut HashSet::new())?)),
            )),
            Terminal::Throw { value, .. } => Some(self.builder.vec1(
                self.builder
                    .statement_throw(SPAN, self.lower_place(value, &mut HashSet::new())?),
            )),
            Terminal::Goto { block, .. } => {
                if let Some(loop_context) = loop_context {
                    if *block == loop_context.continue_target {
                        return Some(self.builder.vec1(self.builder.statement_continue(SPAN, None)));
                    }
                    if *block == loop_context.break_target {
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
                    loop_context,
                )?;
                let consequent = self.wrap_block(consequent);
                let alternate = if *alternate == *fallthrough {
                    None
                } else {
                    Some(self.wrap_block(self.lower_block_sequence(
                        *alternate,
                        Some(*fallthrough),
                        &mut visiting_blocks.clone(),
                        loop_context,
                    )?))
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
                        loop_context,
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
                    Some(LoopContext {
                        continue_target: *test,
                        break_target: *fallthrough,
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
                        loop_context,
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
                    Some(LoopContext {
                        continue_target: *test,
                        break_target: *fallthrough,
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
                        loop_context,
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
                    Some(LoopContext {
                        continue_target,
                        break_target: *fallthrough,
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
                        loop_context,
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
                    loop_context,
                )?;
                let handler_body = self.lower_block_sequence(
                    *handler,
                    Some(*fallthrough),
                    &mut visiting_blocks.clone(),
                    loop_context,
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
                        loop_context,
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
                let mut statements =
                    self.lower_block_sequence(
                        *block,
                        Some(*fallthrough),
                        visiting_blocks,
                        loop_context,
                    )?;
                if Some(*fallthrough) != stop_at {
                    statements.extend(self.lower_block_sequence(
                        *fallthrough,
                        stop_at,
                        visiting_blocks,
                        loop_context,
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
        loop_context: Option<LoopContext>,
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
            loop_context,
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
                Some(Some(self.variable_declaration_statement(name, ast::VariableDeclarationKind::Let, None)))
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
            InstructionValue::CallExpression { .. }
            | InstructionValue::MethodCall { .. }
            | InstructionValue::NewExpression { .. } => {
                if instruction.lvalue.identifier.name.is_some()
                    || self.used_temps.contains(&instruction.lvalue.identifier.id)
                {
                    return Some(None);
                }
                Some(Some(
                    self.builder.statement_expression(
                        SPAN,
                        self.lower_instruction_value(&instruction.value, &mut HashSet::new())?,
                    ),
                ))
            }
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
            InstructionKind::Reassign | InstructionKind::Function | InstructionKind::HoistedFunction => {
                None
            }
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
                        self.builder.simple_assignment_target_assignment_target_identifier(
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
        ast::Statement::VariableDeclaration(self.builder.alloc_variable_declaration(
            SPAN,
            kind,
            self.builder.vec1(self.builder.variable_declarator(
                SPAN,
                kind,
                self.builder
                    .binding_pattern_binding_identifier(SPAN, self.builder.ident(name)),
                NONE,
                init,
                false,
            )),
            false,
        ))
    }

    fn lower_catch_parameter(&self, place: &Place) -> Option<ast::CatchParameter<'a>> {
        let name = place_name(place)?;
        Some(self.builder.catch_parameter(
            SPAN,
            self.builder
                .binding_pattern_binding_identifier(SPAN, self.builder.ident(name)),
            NONE,
        ))
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
        if let Some(name) = place_name(place) {
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
            InstructionValue::StoreLocal { value, .. } | InstructionValue::StoreContext { value, .. } => {
                self.lower_place(value, visiting)
            }
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
            InstructionValue::NewExpression { callee, args, .. } => Some(self.builder.expression_new(
                SPAN,
                self.lower_place(callee, visiting)?,
                NONE,
                self.lower_arguments(args, visiting)?,
            )),
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                receiver_optional,
                call_optional,
                ..
            } => {
                let callee = ast::Expression::from(self.builder.member_expression_computed(
                    SPAN,
                    self.lower_place(receiver, visiting)?,
                    self.lower_place(property, visiting)?,
                    *receiver_optional,
                ));
                Some(self.builder.expression_call(
                    SPAN,
                    callee,
                    NONE,
                    self.lower_arguments(args, visiting)?,
                    *call_optional,
                ))
            }
            InstructionValue::TypeCastExpression { value, .. } => self.lower_place(value, visiting),
            InstructionValue::ArrayExpression { elements, .. } => {
                Some(self.builder.expression_array(SPAN, self.lower_array_elements(elements, visiting)?))
            }
            InstructionValue::ObjectExpression { properties, .. } => {
                Some(self.builder.expression_object(SPAN, self.lower_object_properties(properties, visiting)?))
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
            } => Some(ast::Expression::from(self.builder.member_expression_computed(
                SPAN,
                self.lower_place(object, visiting)?,
                self.lower_place(property, visiting)?,
                *optional,
            ))),
            InstructionValue::MetaProperty { meta, property, .. } => Some(
                self.builder.expression_meta_property(
                    SPAN,
                    self.builder.identifier_name(SPAN, self.builder.ident(meta)),
                    self.builder.identifier_name(SPAN, self.builder.ident(property)),
                ),
            ),
            _ => None,
        }
    }

    fn lower_arguments(
        &self,
        args: &[types::Argument],
        visiting: &mut HashSet<IdentifierId>,
    ) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
        let mut lowered = Vec::with_capacity(args.len());
        for arg in args {
            match arg {
                types::Argument::Place(place) => lowered.push(ast::Argument::from(
                    self.lower_place(place, visiting)?,
                )),
                types::Argument::Spread(place) => lowered.push(
                    self.builder
                        .argument_spread_element(SPAN, self.lower_place(place, visiting)?),
                ),
            }
        }
        Some(self.builder.vec_from_iter(lowered))
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
                types::ArrayElement::Spread(place) => self
                    .builder
                    .array_expression_element_spread_element(SPAN, self.lower_place(place, visiting)?),
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
                    let (key, shorthand, computed) = self.lower_object_property_key(
                        &property.key,
                        &value,
                        visiting,
                    )?;
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
                ast::PropertyKey::from(
                    self.builder
                        .expression_string_literal(SPAN, self.builder.atom(name), None),
                ),
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
            types::ObjectPropertyKey::Computed(place) => {
                Some((ast::PropertyKey::from(self.lower_place(place, visiting)?), false, true))
            }
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
                InstructionValue::StoreLocal { lvalue: types::LValue { kind: InstructionKind::Reassign, .. }, .. }
                    | InstructionValue::StoreContext { lvalue: types::LValue { kind: InstructionKind::Reassign, .. }, .. }
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
            ast::Statement::VariableDeclaration(declaration) => {
                Some(Some(ast::ForStatementInit::VariableDeclaration(declaration)))
            }
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
            _ => Some(Some(
                self.builder
                    .expression_sequence(SPAN, self.builder.vec_from_iter(expressions)),
            )),
        }
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
        types::PropertyLiteral::String(name) if is_identifier_name(name) => Some(
            ast::Expression::from(builder.member_expression_static(
                SPAN,
                object,
                builder.identifier_name(SPAN, builder.ident(name)),
                optional,
            )),
        ),
        types::PropertyLiteral::String(name) => Some(ast::Expression::from(
            builder.member_expression_computed(
                SPAN,
                object,
                builder.expression_string_literal(SPAN, builder.atom(name), None),
                optional,
            ),
        )),
        types::PropertyLiteral::Number(value) => Some(ast::Expression::from(
            builder.member_expression_computed(
                SPAN,
                object,
                builder.expression_numeric_literal(SPAN, *value, None, NumberBase::Decimal),
                optional,
            ),
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
        InstructionValue::StoreLocal { value, .. } | InstructionValue::StoreContext { value, .. } => {
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
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            record_temp_use(object, used);
            record_temp_use(property, used);
        }
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
    if matches!(statements.last(), Some(ast::Statement::ContinueStatement(_))) {
        statements.pop();
    }
}

fn statement_to_expression<'a>(
    statement: ast::Statement<'a>,
    allocator: &'a Allocator,
) -> Option<ast::Expression<'a>> {
    match statement {
        ast::Statement::ExpressionStatement(expr_stmt) => Some(expr_stmt.expression.clone_in(allocator)),
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

fn place_name(place: &Place) -> Option<&str> {
    match place.identifier.name.as_ref()? {
        types::IdentifierName::Named(name) | types::IdentifierName::Promoted(name) => Some(name),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::{
        environment::Environment,
        hir::types::{
            self, BasicBlock, BlockId, DeclarationId, Effect, HIR, HIRFunction, Identifier,
            IdentifierId, IdentifierName, Instruction, InstructionId, InstructionKind,
            MutableRange, Place,
            ReactFunctionType, SourceLocation, Terminal, Type, make_temporary_identifier,
        },
        options::EnvironmentConfig,
    };

    use super::try_lower_function_body;

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

        assert_eq!(try_lower_function_body(&hir).as_deref(), Some("return 1 + 2;\n"));
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

        assert_eq!(try_lower_function_body(&hir).as_deref(), Some("return foo(\"x\");\n"));
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
            Some("let value;\nif (flag) {\n  value = 1;\n} else {\n  value = 2;\n}\nreturn value;\n")
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
            Some("let sum;\nfor (let i = 0; i < 3; i = i + 1) {\n  sum = sum + i;\n}\nreturn sum;\n")
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
            Some("let value;\ntry {\n  throw 1;\n} catch (err) {\n  value = 2;\n}\nreturn value;\n")
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
