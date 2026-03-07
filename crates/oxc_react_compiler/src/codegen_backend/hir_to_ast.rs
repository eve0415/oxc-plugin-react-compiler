use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::{AstBuilder, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_span::{SPAN, SourceType};
use oxc_syntax::{
    identifier::is_identifier_name,
    number::NumberBase,
    operator::{BinaryOperator as AstBinaryOperator, UnaryOperator as AstUnaryOperator},
};

use crate::hir::types::{
    self, HIRFunction, IdentifierId, Instruction, InstructionValue, Place, PrimitiveValue, Terminal,
};

pub(crate) fn try_lower_function_body(hir_function: &HIRFunction) -> Option<String> {
    let (entry_id, entry_block) = hir_function.body.blocks.first()?;
    if *entry_id != hir_function.body.entry || hir_function.body.blocks.len() != 1 {
        return None;
    }
    if !entry_block.phis.is_empty() {
        return None;
    }
    if entry_block
        .instructions
        .iter()
        .any(|instruction| instruction.lvalue.identifier.name.is_some())
    {
        return None;
    }

    let Terminal::Return { value, .. } = &entry_block.terminal else {
        return None;
    };

    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let instruction_map = entry_block
        .instructions
        .iter()
        .map(|instruction| (instruction.lvalue.identifier.id, instruction))
        .collect::<HashMap<_, _>>();

    let return_expression = lower_place(
        builder,
        value,
        &instruction_map,
        &mut HashSet::new(),
    )?;
    let program = builder.program(
        SPAN,
        SourceType::mjs(),
        "",
        builder.vec(),
        None,
        builder.vec(),
        builder.vec1(builder.statement_return(SPAN, Some(return_expression))),
    );
    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        ..CodegenOptions::default()
    };
    Some(Codegen::new().with_options(options).build(&program).code)
}

fn lower_place<'a>(
    builder: AstBuilder<'a>,
    place: &Place,
    instruction_map: &HashMap<IdentifierId, &Instruction>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>> {
    if let Some(name) = place_name(place) {
        return Some(builder.expression_identifier(SPAN, builder.ident(name)));
    }

    let identifier_id = place.identifier.id;
    if !visiting.insert(identifier_id) {
        return None;
    }
    let instruction = instruction_map.get(&identifier_id)?;
    let expression = lower_instruction_value(builder, &instruction.value, instruction_map, visiting);
    visiting.remove(&identifier_id);
    expression
}

fn lower_instruction_value<'a>(
    builder: AstBuilder<'a>,
    value: &InstructionValue,
    instruction_map: &HashMap<IdentifierId, &Instruction>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>> {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            lower_place(builder, place, instruction_map, visiting)
        }
        InstructionValue::Primitive { value, .. } => Some(lower_primitive(builder, value)),
        InstructionValue::BinaryExpression {
            operator,
            left,
            right,
            ..
        } => Some(builder.expression_binary(
            SPAN,
            lower_place(builder, left, instruction_map, visiting)?,
            lower_binary_operator(*operator),
            lower_place(builder, right, instruction_map, visiting)?,
        )),
        InstructionValue::UnaryExpression {
            operator, value, ..
        } => Some(builder.expression_unary(
            SPAN,
            lower_unary_operator(*operator),
            lower_place(builder, value, instruction_map, visiting)?,
        )),
        InstructionValue::PropertyLoad {
            object,
            property,
            optional,
            ..
        } => Some(lower_property_load(
            builder,
            object,
            property,
            *optional,
            instruction_map,
            visiting,
        )?),
        InstructionValue::ComputedLoad {
            object,
            property,
            optional,
            ..
        } => Some(ast::Expression::from(builder.member_expression_computed(
            SPAN,
            lower_place(builder, object, instruction_map, visiting)?,
            lower_place(builder, property, instruction_map, visiting)?,
            *optional,
        ))),
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => Some(builder.expression_call(
            SPAN,
            lower_place(builder, callee, instruction_map, visiting)?,
            None::<oxc_allocator::Box<'a, ast::TSTypeParameterInstantiation<'a>>>,
            lower_arguments(builder, args, instruction_map, visiting)?,
            *optional,
        )),
        _ => None,
    }
}

fn lower_arguments<'a>(
    builder: AstBuilder<'a>,
    args: &[types::Argument],
    instruction_map: &HashMap<IdentifierId, &Instruction>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
    let mut lowered = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            types::Argument::Place(place) => lowered.push(ast::Argument::from(lower_place(
                builder,
                place,
                instruction_map,
                visiting,
            )?)),
            types::Argument::Spread(place) => lowered.push(builder.argument_spread_element(
                SPAN,
                lower_place(builder, place, instruction_map, visiting)?,
            )),
        }
    }
    Some(builder.vec_from_iter(lowered))
}

fn lower_property_load<'a>(
    builder: AstBuilder<'a>,
    object: &Place,
    property: &types::PropertyLiteral,
    optional: bool,
    instruction_map: &HashMap<IdentifierId, &Instruction>,
    visiting: &mut HashSet<IdentifierId>,
) -> Option<ast::Expression<'a>> {
    let object = lower_place(builder, object, instruction_map, visiting)?;
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
            IdentifierId, IdentifierName, Instruction, InstructionId, MutableRange, Place,
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
