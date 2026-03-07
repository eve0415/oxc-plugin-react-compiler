//! Inline JSX transform.
//!
//! Port of upstream `Optimization/InlineJsxTransform.ts`.
//! This pass rewrites JSX instructions into `if (DEV) { jsx } else { object-literal }`
//! form on HIR before reactive codegen.

use std::collections::{HashMap, HashSet};

use crate::hir::builder::{mark_predecessors, reverse_postorder_blocks};
use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;
use crate::hir::visitors::{
    for_each_instruction_lvalue, for_each_instruction_operand, for_each_terminal_operand,
    map_instruction_lvalues, map_instruction_operands, map_terminal_operands,
};
use crate::options::InlineJsxTransformConfig;

#[derive(Debug, Clone)]
struct InlinedJsxDeclaration {
    identifier: Identifier,
    block_ids_to_ignore: HashSet<BlockId>,
}

type InlinedJsxDeclarationMap = HashMap<DeclarationId, InlinedJsxDeclaration>;

pub fn inline_jsx_transform(func: &mut HIRFunction, config: &InlineJsxTransformConfig) {
    ensure_fresh_ids(func);

    let mut inlined_jsx_declarations: InlinedJsxDeclarationMap = HashMap::new();

    // Step 1: Generate conditional JSX/object-literal blocks.
    let block_ids: Vec<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();
    for block_id in block_ids {
        let Some(block_index) = find_block_index(&func.body.blocks, block_id) else {
            continue;
        };
        let block_kind = func.body.blocks[block_index].1.kind;
        let instruction_count = func.body.blocks[block_index].1.instructions.len();

        for i in 0..instruction_count {
            let Some(current_index) = find_block_index(&func.body.blocks, block_id) else {
                break;
            };
            if i >= func.body.blocks[current_index].1.instructions.len() {
                break;
            }

            // Recurse into nested lowered functions first.
            {
                let instr = &mut func.body.blocks[current_index].1.instructions[i];
                match &mut instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        inline_jsx_transform(&mut lowered_func.func, config);
                    }
                    _ => {}
                }
            }

            let instr = func.body.blocks[current_index].1.instructions[i].clone();
            let is_jsx = matches!(
                instr.value,
                InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
            );
            if !is_jsx {
                continue;
            }

            // TODO parity: upstream logs a TODO diagnostic for value blocks.
            if block_kind == BlockKind::Value {
                continue;
            }

            rewrite_single_jsx_instruction(
                func,
                current_index,
                i,
                &instr,
                config,
                &mut inlined_jsx_declarations,
            );
            break;
        }
    }

    // Step 2: Replace declaration references with the new phi-backed identifiers.
    for (block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            map_instruction_operands(instr, |place| {
                handle_place(place, *block_id, &inlined_jsx_declarations)
            });
            map_instruction_lvalues(instr, |place| {
                handle_lvalue(place, *block_id, &inlined_jsx_declarations)
            });
        }

        map_terminal_operands(&mut block.terminal, |place| {
            handle_place(place, *block_id, &inlined_jsx_declarations)
        });

        if let Terminal::Scope { scope, .. } | Terminal::PrunedScope { scope, .. } =
            &mut block.terminal
        {
            for dep in &mut scope.dependencies {
                dep.identifier = handle_identifier(&dep.identifier, &inlined_jsx_declarations);
            }

            let existing: Vec<(IdentifierId, ScopeDeclaration)> = scope
                .declarations
                .iter()
                .map(|(id, decl)| (*id, decl.clone()))
                .collect();
            for (orig_id, mut decl) in existing {
                let new_decl = handle_identifier(&decl.identifier, &inlined_jsx_declarations);
                if new_decl.id != orig_id {
                    scope.declarations.remove(&orig_id);
                    decl.identifier = new_decl.clone();
                    scope.declarations.insert(
                        decl.identifier.id,
                        ScopeDeclaration {
                            identifier: decl.identifier,
                            scope: decl.scope,
                        },
                    );
                }
            }
        }
    }

    // Step 3: Re-normalize CFG ordering / IDs and fix ranges.
    reverse_postorder_blocks(&mut func.body);
    mark_predecessors(&mut func.body);
    mark_instruction_ids(&mut func.body);
    fix_scope_and_identifier_ranges(&mut func.body);
}

fn rewrite_single_jsx_instruction(
    func: &mut HIRFunction,
    block_index: usize,
    i: usize,
    instr: &Instruction,
    config: &InlineJsxTransformConfig,
    inlined_jsx_declarations: &mut InlinedJsxDeclarationMap,
) {
    let current_block = func.body.blocks[block_index].1.clone();
    let instr_loc = instr.loc.clone();
    let instr_value_loc = instr.value.loc().clone();

    let mut current_block_instructions = current_block.instructions[..i].to_vec();
    let mut then_block_instructions = current_block.instructions[i..i + 1].to_vec();
    let mut else_block_instructions: Vec<Instruction> = Vec::new();
    let fallthrough_block_instructions = current_block.instructions[i + 1..].to_vec();

    let fallthrough_block_id = BlockId(func.env.next_block_id());
    let mut fallthrough_block = BasicBlock {
        kind: current_block.kind,
        id: fallthrough_block_id,
        instructions: fallthrough_block_instructions,
        terminal: current_block.terminal.clone(),
        preds: HashSet::new(),
        phis: Vec::new(),
    };

    // Current block: declare temp + load DEV global + if terminal.
    let mut var_place = create_temporary_place(&func.env, instr.value.loc().clone());
    promote_temporary(&mut var_place.identifier);
    let var_lvalue_place = create_temporary_place(&func.env, instr.value.loc().clone());
    let then_var_place = Place {
        identifier: fork_temporary_identifier(&func.env, &var_place.identifier),
        ..var_place.clone()
    };
    let else_var_place = Place {
        identifier: fork_temporary_identifier(&func.env, &var_place.identifier),
        ..var_place.clone()
    };

    let var_instruction = Instruction {
        id: make_instruction_id(0),
        lvalue: var_lvalue_place,
        value: InstructionValue::DeclareLocal {
            lvalue: LValue {
                place: var_place.clone(),
                kind: InstructionKind::Let,
            },
            loc: instr_value_loc.clone(),
        },
        effects: None,
        loc: instr_loc.clone(),
    };
    current_block_instructions.push(var_instruction);

    let mut dev_global_place = create_temporary_place(&func.env, instr.value.loc().clone());
    dev_global_place.effect = Effect::Mutate;
    let dev_global_instruction = Instruction {
        id: make_instruction_id(0),
        lvalue: dev_global_place.clone(),
        value: InstructionValue::LoadGlobal {
            binding: NonLocalBinding::Global {
                name: config.global_dev_var.clone(),
            },
            loc: instr_value_loc.clone(),
        },
        effects: None,
        loc: instr_loc.clone(),
    };
    current_block_instructions.push(dev_global_instruction);

    let then_block_id = BlockId(func.env.next_block_id());
    let else_block_id = BlockId(func.env.next_block_id());
    let mut dev_test = dev_global_place.clone();
    dev_test.effect = Effect::Read;
    let if_terminal = Terminal::If {
        test: dev_test,
        consequent: then_block_id,
        alternate: else_block_id,
        fallthrough: fallthrough_block_id,
        id: make_instruction_id(0),
        loc: instr_loc.clone(),
    };

    func.body.blocks[block_index].1.instructions = current_block_instructions;
    func.body.blocks[block_index].1.terminal = if_terminal;

    // Then block keeps original JSX and reassigns branch value.
    let mut then_block = BasicBlock {
        id: then_block_id,
        instructions: Vec::new(),
        kind: BlockKind::Block,
        phis: Vec::new(),
        preds: HashSet::new(),
        terminal: Terminal::Goto {
            block: fallthrough_block_id,
            variant: GotoVariant::Break,
            id: make_instruction_id(0),
            loc: instr_loc.clone(),
        },
    };
    then_block.instructions.append(&mut then_block_instructions);
    let reassign_then_instruction = Instruction {
        id: make_instruction_id(0),
        lvalue: create_temporary_place(&func.env, instr.value.loc().clone()),
        value: InstructionValue::StoreLocal {
            lvalue: LValue {
                place: else_var_place.clone(),
                kind: InstructionKind::Reassign,
            },
            value: instr.lvalue.clone(),
            loc: instr_value_loc.clone(),
        },
        effects: None,
        loc: instr_loc.clone(),
    };
    then_block.instructions.push(reassign_then_instruction);

    // Else block contains object-literal ReactElement construction.
    let mut else_block = BasicBlock {
        id: else_block_id,
        instructions: Vec::new(),
        kind: BlockKind::Block,
        phis: Vec::new(),
        preds: HashSet::new(),
        terminal: Terminal::Goto {
            block: fallthrough_block_id,
            variant: GotoVariant::Break,
            id: make_instruction_id(0),
            loc: instr_loc.clone(),
        },
    };

    let (ref_property, key_property, props_property, type_property) = match &instr.value {
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            let (ref_prop, key_prop, props_prop) = create_props_properties(
                func,
                instr,
                &mut else_block_instructions,
                props,
                children.as_deref(),
            );
            let tag_prop = create_tag_property(func, instr, &mut else_block_instructions, tag);
            (ref_prop, key_prop, props_prop, tag_prop)
        }
        InstructionValue::JsxFragment { children, .. } => {
            let empty_props: [JsxAttribute; 0] = [];
            let (ref_prop, key_prop, props_prop) = create_props_properties(
                func,
                instr,
                &mut else_block_instructions,
                &empty_props,
                Some(children.as_slice()),
            );
            let tag_prop = create_symbol_property(
                func,
                instr,
                &mut else_block_instructions,
                "type",
                "react.fragment",
            );
            (ref_prop, key_prop, props_prop, tag_prop)
        }
        _ => return,
    };

    let mut react_element_place = create_temporary_place(&func.env, instr.value.loc().clone());
    react_element_place.effect = Effect::Store;
    let react_element_instruction = Instruction {
        id: make_instruction_id(0),
        lvalue: react_element_place.clone(),
        value: InstructionValue::ObjectExpression {
            properties: vec![
                ObjectPropertyOrSpread::Property(create_symbol_property(
                    func,
                    instr,
                    &mut else_block_instructions,
                    "$$typeof",
                    &config.element_symbol,
                )),
                ObjectPropertyOrSpread::Property(type_property),
                ObjectPropertyOrSpread::Property(ref_property),
                ObjectPropertyOrSpread::Property(key_property),
                ObjectPropertyOrSpread::Property(props_property),
            ],
            loc: instr_value_loc.clone(),
        },
        effects: None,
        loc: instr_loc.clone(),
    };
    else_block_instructions.push(react_element_instruction.clone());

    let reassign_else_instruction = Instruction {
        id: make_instruction_id(0),
        lvalue: create_temporary_place(&func.env, instr.value.loc().clone()),
        value: InstructionValue::StoreLocal {
            lvalue: LValue {
                place: else_var_place.clone(),
                kind: InstructionKind::Reassign,
            },
            value: react_element_instruction.lvalue,
            loc: instr_value_loc.clone(),
        },
        effects: None,
        loc: instr_loc.clone(),
    };
    else_block_instructions.push(reassign_else_instruction);
    else_block.instructions.append(&mut else_block_instructions);

    // Merge branch values with a phi in fallthrough.
    let mut operands = HashMap::new();
    operands.insert(then_block_id, else_var_place);
    operands.insert(else_block_id, then_var_place);

    let phi_identifier = fork_temporary_identifier(&func.env, &var_place.identifier);
    let mut phi_place = create_temporary_place(&func.env, instr.value.loc().clone());
    phi_place.identifier = phi_identifier.clone();
    fallthrough_block.phis = vec![Phi {
        operands,
        place: phi_place,
    }];

    insert_or_replace_block(&mut func.body, then_block);
    insert_or_replace_block(&mut func.body, else_block);
    insert_or_replace_block(&mut func.body, fallthrough_block);

    let mut block_ids_to_ignore = HashSet::new();
    block_ids_to_ignore.insert(then_block_id);
    block_ids_to_ignore.insert(else_block_id);
    inlined_jsx_declarations.insert(
        instr.lvalue.identifier.declaration_id,
        InlinedJsxDeclaration {
            identifier: phi_identifier,
            block_ids_to_ignore,
        },
    );
}

fn create_symbol_property(
    func: &mut HIRFunction,
    instr: &Instruction,
    next_instructions: &mut Vec<Instruction>,
    property_name: &str,
    symbol_name: &str,
) -> ObjectProperty {
    let mut symbol_place = create_temporary_place(&func.env, instr.value.loc().clone());
    symbol_place.effect = Effect::Mutate;
    next_instructions.push(Instruction {
        id: make_instruction_id(0),
        lvalue: symbol_place.clone(),
        value: InstructionValue::LoadGlobal {
            binding: NonLocalBinding::Global {
                name: "Symbol".to_string(),
            },
            loc: instr.value.loc().clone(),
        },
        effects: None,
        loc: instr.loc.clone(),
    });

    let mut symbol_for_place = create_temporary_place(&func.env, instr.value.loc().clone());
    symbol_for_place.effect = Effect::Read;
    next_instructions.push(Instruction {
        id: make_instruction_id(0),
        lvalue: symbol_for_place.clone(),
        value: InstructionValue::PropertyLoad {
            object: symbol_place.clone(),
            property: PropertyLiteral::String("for".to_string()),
            optional: false,
            loc: instr.value.loc().clone(),
        },
        effects: None,
        loc: instr.loc.clone(),
    });

    let mut symbol_value_place = create_temporary_place(&func.env, instr.value.loc().clone());
    symbol_value_place.effect = Effect::Mutate;
    next_instructions.push(Instruction {
        id: make_instruction_id(0),
        lvalue: symbol_value_place.clone(),
        value: InstructionValue::Primitive {
            value: PrimitiveValue::String(symbol_name.to_string()),
            loc: instr.value.loc().clone(),
        },
        effects: None,
        loc: instr.loc.clone(),
    });

    let mut result_place = create_temporary_place(&func.env, instr.value.loc().clone());
    result_place.effect = Effect::Mutate;
    next_instructions.push(Instruction {
        id: make_instruction_id(0),
        lvalue: result_place.clone(),
        value: InstructionValue::MethodCall {
            receiver: symbol_place,
            property: symbol_for_place,
            args: vec![Argument::Place(symbol_value_place)],
            receiver_optional: false,
            call_optional: false,
            loc: instr.value.loc().clone(),
        },
        effects: None,
        loc: instr.loc.clone(),
    });

    let mut captured = result_place;
    captured.effect = Effect::Capture;
    ObjectProperty {
        key: ObjectPropertyKey::String(property_name.to_string()),
        type_: ObjectPropertyType::Property,
        place: captured,
    }
}

fn create_tag_property(
    func: &mut HIRFunction,
    instr: &Instruction,
    next_instructions: &mut Vec<Instruction>,
    component_tag: &JsxTag,
) -> ObjectProperty {
    match component_tag {
        JsxTag::BuiltinTag(name) => {
            let mut tag_place = create_temporary_place(&func.env, instr.value.loc().clone());
            tag_place.effect = Effect::Mutate;
            next_instructions.push(Instruction {
                id: make_instruction_id(0),
                lvalue: tag_place.clone(),
                value: InstructionValue::Primitive {
                    value: PrimitiveValue::String(name.clone()),
                    loc: instr.value.loc().clone(),
                },
                effects: None,
                loc: instr.loc.clone(),
            });
            let mut captured = tag_place;
            captured.effect = Effect::Capture;
            ObjectProperty {
                key: ObjectPropertyKey::String("type".to_string()),
                type_: ObjectPropertyType::Property,
                place: captured,
            }
        }
        JsxTag::Component(tag_place) => {
            let mut captured = tag_place.clone();
            captured.effect = Effect::Capture;
            ObjectProperty {
                key: ObjectPropertyKey::String("type".to_string()),
                type_: ObjectPropertyType::Property,
                place: captured,
            }
        }
        JsxTag::Fragment => {
            create_symbol_property(func, instr, next_instructions, "type", "react.fragment")
        }
    }
}

fn create_props_properties(
    func: &mut HIRFunction,
    instr: &Instruction,
    next_instructions: &mut Vec<Instruction>,
    prop_attributes: &[JsxAttribute],
    children: Option<&[Place]>,
) -> (ObjectProperty, ObjectProperty, ObjectProperty) {
    let mut ref_property: Option<ObjectProperty> = None;
    let mut key_property: Option<ObjectProperty> = None;
    let mut props: Vec<ObjectPropertyOrSpread> = Vec::new();

    let non_key_attr_count = prop_attributes
        .iter()
        .filter(|p| matches!(p, JsxAttribute::Attribute { name, .. } if name != "key"))
        .count();
    let spread_attrs: Vec<&Place> = prop_attributes
        .iter()
        .filter_map(|p| match p {
            JsxAttribute::SpreadAttribute { argument } => Some(argument),
            _ => None,
        })
        .collect();
    let spread_props_only = non_key_attr_count == 0 && spread_attrs.len() == 1;

    for prop in prop_attributes {
        match prop {
            JsxAttribute::Attribute { name, place } => match name.as_str() {
                "key" => {
                    key_property = Some(ObjectProperty {
                        key: ObjectPropertyKey::String("key".to_string()),
                        type_: ObjectPropertyType::Property,
                        place: place.clone(),
                    });
                }
                "ref" => {
                    let ref_prop = ObjectProperty {
                        key: ObjectPropertyKey::String("ref".to_string()),
                        type_: ObjectPropertyType::Property,
                        place: place.clone(),
                    };
                    ref_property = Some(ref_prop.clone());
                    props.push(ObjectPropertyOrSpread::Property(ref_prop));
                }
                _ => {
                    props.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key: ObjectPropertyKey::String(name.clone()),
                        type_: ObjectPropertyType::Property,
                        place: place.clone(),
                    }));
                }
            },
            JsxAttribute::SpreadAttribute { argument } => {
                props.push(ObjectPropertyOrSpread::Spread(argument.clone()));
            }
        }
    }

    if let Some(children) = children {
        if children.len() == 1 {
            let mut child = children[0].clone();
            child.effect = Effect::Capture;
            props.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                key: ObjectPropertyKey::String("children".to_string()),
                type_: ObjectPropertyType::Property,
                place: child,
            }));
        } else if !children.is_empty() {
            let mut children_place = create_temporary_place(&func.env, instr.value.loc().clone());
            children_place.effect = Effect::Mutate;
            let elements = children.iter().cloned().map(ArrayElement::Place).collect();
            next_instructions.push(Instruction {
                id: make_instruction_id(0),
                lvalue: children_place.clone(),
                value: InstructionValue::ArrayExpression {
                    elements,
                    loc: instr.value.loc().clone(),
                },
                effects: None,
                loc: instr.loc.clone(),
            });
            let mut captured = children_place;
            captured.effect = Effect::Capture;
            props.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                key: ObjectPropertyKey::String("children".to_string()),
                type_: ObjectPropertyType::Property,
                place: captured,
            }));
        }
    }

    let ref_property = ref_property.unwrap_or_else(|| {
        let mut ref_place = create_temporary_place(&func.env, instr.value.loc().clone());
        ref_place.effect = Effect::Mutate;
        next_instructions.push(Instruction {
            id: make_instruction_id(0),
            lvalue: ref_place.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: instr.value.loc().clone(),
            },
            effects: None,
            loc: instr.loc.clone(),
        });
        let mut captured = ref_place;
        captured.effect = Effect::Capture;
        ObjectProperty {
            key: ObjectPropertyKey::String("ref".to_string()),
            type_: ObjectPropertyType::Property,
            place: captured,
        }
    });

    let key_property = key_property.unwrap_or_else(|| {
        let mut key_place = create_temporary_place(&func.env, instr.value.loc().clone());
        key_place.effect = Effect::Mutate;
        next_instructions.push(Instruction {
            id: make_instruction_id(0),
            lvalue: key_place.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: instr.value.loc().clone(),
            },
            effects: None,
            loc: instr.loc.clone(),
        });
        let mut captured = key_place;
        captured.effect = Effect::Capture;
        ObjectProperty {
            key: ObjectPropertyKey::String("key".to_string()),
            type_: ObjectPropertyType::Property,
            place: captured,
        }
    });

    let props_property = if spread_props_only {
        let mut spread_arg = spread_attrs[0].clone();
        spread_arg.effect = Effect::Mutate;
        ObjectProperty {
            key: ObjectPropertyKey::String("props".to_string()),
            type_: ObjectPropertyType::Property,
            place: spread_arg,
        }
    } else {
        let mut props_place = create_temporary_place(&func.env, instr.value.loc().clone());
        props_place.effect = Effect::Mutate;
        next_instructions.push(Instruction {
            id: make_instruction_id(0),
            lvalue: props_place.clone(),
            value: InstructionValue::ObjectExpression {
                properties: props,
                loc: instr.value.loc().clone(),
            },
            effects: None,
            loc: instr.loc.clone(),
        });
        let mut captured = props_place;
        captured.effect = Effect::Capture;
        ObjectProperty {
            key: ObjectPropertyKey::String("props".to_string()),
            type_: ObjectPropertyType::Property,
            place: captured,
        }
    };

    (ref_property, key_property, props_property)
}

fn handle_place(
    place: &mut Place,
    block_id: BlockId,
    inlined_jsx_declarations: &InlinedJsxDeclarationMap,
) {
    let Some(decl) = inlined_jsx_declarations.get(&place.identifier.declaration_id) else {
        return;
    };
    if decl.block_ids_to_ignore.contains(&block_id) {
        return;
    }
    place.identifier = decl.identifier.clone();
}

fn handle_lvalue(
    lvalue: &mut Place,
    block_id: BlockId,
    inlined_jsx_declarations: &InlinedJsxDeclarationMap,
) {
    let Some(decl) = inlined_jsx_declarations.get(&lvalue.identifier.declaration_id) else {
        return;
    };
    if decl.block_ids_to_ignore.contains(&block_id) {
        return;
    }
    lvalue.identifier = decl.identifier.clone();
}

fn handle_identifier(
    identifier: &Identifier,
    inlined_jsx_declarations: &InlinedJsxDeclarationMap,
) -> Identifier {
    inlined_jsx_declarations
        .get(&identifier.declaration_id)
        .map(|decl| decl.identifier.clone())
        .unwrap_or_else(|| identifier.clone())
}

fn find_block_index(blocks: &[(BlockId, BasicBlock)], block_id: BlockId) -> Option<usize> {
    blocks.iter().position(|(id, _)| *id == block_id)
}

fn insert_or_replace_block(body: &mut HIR, block: BasicBlock) {
    if let Some(index) = find_block_index(&body.blocks, block.id) {
        body.blocks[index] = (block.id, block);
    } else {
        body.blocks.push((block.id, block));
    }
}

fn create_temporary_place(env: &crate::environment::Environment, loc: SourceLocation) -> Place {
    Place {
        identifier: env.make_temporary_identifier(loc),
        effect: Effect::Unknown,
        reactive: false,
        loc: SourceLocation::Generated,
    }
}

fn promote_temporary(identifier: &mut Identifier) {
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
}

fn fork_temporary_identifier(
    env: &crate::environment::Environment,
    source: &Identifier,
) -> Identifier {
    let mut cloned = source.clone();
    cloned.id = IdentifierId::new(env.next_identifier_id());
    cloned.mutable_range = MutableRange::default();
    cloned
}

fn fix_scope_and_identifier_ranges(body: &mut HIR) {
    let mut first_ids_by_block: HashMap<BlockId, InstructionId> = HashMap::new();
    for (bid, block) in &body.blocks {
        let first_id = block
            .instructions
            .first()
            .map(|i| i.id)
            .unwrap_or_else(|| block.terminal.id());
        first_ids_by_block.insert(*bid, first_id);
    }

    let mut scope_ranges: HashMap<ScopeId, MutableRange> = HashMap::new();
    for (_, block) in &body.blocks {
        match &block.terminal {
            Terminal::Scope {
                id,
                fallthrough,
                scope,
                ..
            }
            | Terminal::PrunedScope {
                id,
                fallthrough,
                scope,
                ..
            } => {
                if let Some(first_id) = first_ids_by_block.get(fallthrough) {
                    scope_ranges.insert(
                        scope.id,
                        MutableRange {
                            start: *id,
                            end: *first_id,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    if scope_ranges.is_empty() {
        return;
    }

    for (_, block) in &mut body.blocks {
        for phi in &mut block.phis {
            update_identifier_scope_range(&mut phi.place.identifier, &scope_ranges);
            for operand in phi.operands.values_mut() {
                update_identifier_scope_range(&mut operand.identifier, &scope_ranges);
            }
        }

        for instr in &mut block.instructions {
            map_instruction_lvalues(instr, |place| {
                update_identifier_scope_range(&mut place.identifier, &scope_ranges)
            });
            map_instruction_operands(instr, |place| {
                update_identifier_scope_range(&mut place.identifier, &scope_ranges)
            });
        }

        if let Terminal::Scope { scope, .. } | Terminal::PrunedScope { scope, .. } =
            &mut block.terminal
            && let Some(range) = scope_ranges.get(&scope.id)
        {
            scope.range = range.clone();
        }

        map_terminal_operands(&mut block.terminal, |place| {
            update_identifier_scope_range(&mut place.identifier, &scope_ranges)
        });
    }
}

fn update_identifier_scope_range(
    identifier: &mut Identifier,
    scope_ranges: &HashMap<ScopeId, MutableRange>,
) {
    if let Some(scope) = &mut identifier.scope
        && let Some(range) = scope_ranges.get(&scope.id)
    {
        scope.range = range.clone();
        identifier.mutable_range = range.clone();
    }
}

fn ensure_fresh_ids(func: &HIRFunction) {
    let mut max_block = 0u32;
    for (bid, _) in &func.body.blocks {
        max_block = max_block.max(bid.0);
    }

    let mut max_ident = 0u32;
    let mut bump_ident = |identifier: &Identifier| {
        max_ident = max_ident.max(identifier.id.0);
    };

    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => bump_ident(&place.identifier),
        }
    }
    bump_ident(&func.returns.identifier);
    for place in &func.context {
        bump_ident(&place.identifier);
    }

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            bump_ident(&phi.place.identifier);
            for operand in phi.operands.values() {
                bump_ident(&operand.identifier);
            }
        }

        for instr in &block.instructions {
            bump_ident(&instr.lvalue.identifier);
            for_each_instruction_lvalue(instr, |place| bump_ident(&place.identifier));
            for_each_instruction_operand(instr, |place| bump_ident(&place.identifier));
        }
        for_each_terminal_operand(&block.terminal, |place| bump_ident(&place.identifier));

        if let Terminal::Scope { scope, .. } | Terminal::PrunedScope { scope, .. } = &block.terminal
        {
            for dep in &scope.dependencies {
                bump_ident(&dep.identifier);
            }
            for decl in scope.declarations.values() {
                bump_ident(&decl.identifier);
            }
            for reassignment in &scope.reassignments {
                bump_ident(reassignment);
            }
            if let Some(early) = &scope.early_return_value {
                bump_ident(&early.value);
            }
        }
    }

    let desired_next_block = max_block.saturating_add(1);
    if desired_next_block > func.env.current_next_block_id() {
        func.env.set_next_block_id(desired_next_block);
    }

    let desired_next_ident = max_ident.saturating_add(1);
    if desired_next_ident > func.env.current_next_identifier_id() {
        func.env.set_next_identifier_id(desired_next_ident);
    }
}
