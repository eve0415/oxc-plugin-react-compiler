//! Port of `Optimization/InstructionReordering.ts` from upstream React Compiler.
//!
//! This pass conservatively reorders instructions to move reorderable values
//! closer to their use sites. It improves downstream scope merge opportunities.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use indexmap::{IndexMap, IndexSet};

use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;
use crate::hir::visitors::{
    for_each_instruction_lvalue, for_each_instruction_value_operand, for_each_pattern_place,
    for_each_terminal_operand,
};

type Nodes = IndexMap<IdentifierId, Node>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reorderability {
    Reorderable,
    Nonreorderable,
}

#[derive(Debug, Clone)]
struct Node {
    instruction: Option<Instruction>,
    dependencies: IndexSet<IdentifierId>,
    reorderability: Reorderability,
    depth: Option<usize>,
}

impl Node {
    fn for_instruction(instruction: Instruction, reorderability: Reorderability) -> Self {
        Self {
            instruction: Some(instruction),
            dependencies: IndexSet::new(),
            reorderability,
            depth: None,
        }
    }

    fn for_lvalue_only() -> Self {
        Self {
            instruction: None,
            dependencies: IndexSet::new(),
            reorderability: Reorderability::Nonreorderable,
            depth: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ReferenceKind {
    Read,
    Write,
}

#[derive(Debug, Default)]
struct References {
    single_use_identifiers: IndexSet<IdentifierId>,
    last_assignments: IndexMap<String, InstructionId>,
}

fn named_identifier(name: &Option<IdentifierName>) -> Option<&str> {
    match name {
        Some(IdentifierName::Named(v)) => Some(v.as_str()),
        _ => None,
    }
}

fn is_expression_block_kind(kind: BlockKind) -> bool {
    !matches!(kind, BlockKind::Block | BlockKind::Catch)
}

/// Reorder instructions to match upstream `instructionReordering`.
pub fn instruction_reordering(func: &mut HIRFunction) {
    // Shared nodes are emitted when first referenced by a later block.
    let mut shared: Nodes = IndexMap::new();
    let references = find_referenced_range_of_temporaries(func);

    for (_, block) in &mut func.body.blocks {
        reorder_block(block, &mut shared, &references);
    }

    assert!(
        shared.is_empty(),
        "InstructionReordering: expected all reorderable nodes to be emitted; leftover={}",
        shared.len()
    );

    mark_instruction_ids(&mut func.body);
}

fn find_referenced_range_of_temporaries(func: &HIRFunction) -> References {
    let mut single_use_counts: IndexMap<IdentifierId, usize> = IndexMap::new();
    let mut last_assignments: IndexMap<String, InstructionId> = IndexMap::new();

    let mut reference = |instr_id: InstructionId, place: &Place, kind: ReferenceKind| {
        if let Some(name) = named_identifier(&place.identifier.name) {
            if matches!(kind, ReferenceKind::Write) {
                match last_assignments.get(name).copied() {
                    Some(previous) if previous.0 >= instr_id.0 => {}
                    _ => {
                        last_assignments.insert(name.to_string(), instr_id);
                    }
                }
            }
            return;
        }

        if matches!(kind, ReferenceKind::Read) {
            let next = single_use_counts
                .get(&place.identifier.id)
                .copied()
                .unwrap_or(0)
                + 1;
            single_use_counts.insert(place.identifier.id, next);
        }
    };

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            for_each_instruction_value_lvalue(&instr.value, |place| {
                reference(instr.id, place, ReferenceKind::Read);
            });
            for_each_instruction_lvalue(instr, |place| {
                reference(instr.id, place, ReferenceKind::Write);
            });
        }
        let terminal_id = get_terminal_id(&block.terminal);
        for_each_terminal_operand(&block.terminal, |operand| {
            reference(terminal_id, operand, ReferenceKind::Read);
        });
    }

    let single_use_identifiers = single_use_counts
        .into_iter()
        .filter_map(|(id, count)| (count == 1).then_some(id))
        .collect::<IndexSet<_>>();

    References {
        single_use_identifiers,
        last_assignments,
    }
}

fn reorder_block(block: &mut BasicBlock, shared: &mut Nodes, references: &References) {
    let mut locals: Nodes = IndexMap::new();
    let mut named: IndexMap<String, IdentifierId> = IndexMap::new();
    let mut previous_non_reorderable: Option<IdentifierId> = None;

    for instr in &block.instructions {
        let lvalue_id = instr.lvalue.identifier.id;
        let reorderability = get_reorderability(instr, references);

        {
            let node = locals
                .entry(lvalue_id)
                .or_insert_with(|| Node::for_instruction(instr.clone(), reorderability));
            if node.instruction.is_none() {
                node.instruction = Some(instr.clone());
            }
            node.reorderability = reorderability;

            if matches!(reorderability, Reorderability::Nonreorderable) {
                if let Some(previous) = previous_non_reorderable {
                    node.dependencies.insert(previous);
                }
                previous_non_reorderable = Some(lvalue_id);
            }
        }

        let mut dependencies: Vec<IdentifierId> = Vec::new();
        for_each_instruction_value_operand(&instr.value, |operand| {
            if let Some(name) = named_identifier(&operand.identifier.name) {
                if let Some(previous) = named.get(name).copied() {
                    dependencies.push(previous);
                }
                named.insert(name.to_string(), lvalue_id);
            } else if locals.contains_key(&operand.identifier.id)
                || shared.contains_key(&operand.identifier.id)
            {
                dependencies.push(operand.identifier.id);
            }
        });
        if let Some(node) = locals.get_mut(&lvalue_id) {
            for dep in dependencies {
                node.dependencies.insert(dep);
            }
        }

        let mut value_lvalues: Vec<(IdentifierId, Option<String>)> = Vec::new();
        for_each_instruction_value_lvalue(&instr.value, |place| {
            value_lvalues.push((
                place.identifier.id,
                named_identifier(&place.identifier.name).map(str::to_string),
            ));
        });
        for (value_lvalue_id, value_lvalue_name) in value_lvalues {
            let lvalue_node = locals
                .entry(value_lvalue_id)
                .or_insert_with(Node::for_lvalue_only);
            lvalue_node.dependencies.insert(lvalue_id);

            if let Some(name) = value_lvalue_name {
                if let Some(previous) = named.get(&name).copied()
                    && let Some(node) = locals.get_mut(&lvalue_id)
                {
                    node.dependencies.insert(previous);
                }
                named.insert(name, lvalue_id);
            }
        }
    }

    let mut next_instructions: Vec<Instruction> = Vec::new();

    if is_expression_block_kind(block.kind) {
        if let Some(previous) = previous_non_reorderable {
            emit(&mut locals, shared, &mut next_instructions, previous);
        }

        if let Some(last) = block.instructions.last() {
            emit(
                &mut locals,
                shared,
                &mut next_instructions,
                last.lvalue.identifier.id,
            );
        }

        let mut terminal_operands: Vec<IdentifierId> = Vec::new();
        for_each_terminal_operand(&block.terminal, |operand| {
            terminal_operands.push(operand.identifier.id);
        });
        for operand in terminal_operands {
            emit(&mut locals, shared, &mut next_instructions, operand);
        }

        let remaining: Vec<(IdentifierId, Node)> = locals
            .iter()
            .map(|(id, node)| (*id, node.clone()))
            .collect();
        for (id, node) in remaining {
            if node.instruction.is_none() {
                continue;
            }
            assert!(
                matches!(node.reorderability, Reorderability::Reorderable),
                "InstructionReordering: remaining instruction is not reorderable"
            );
            shared.insert(id, node);
        }
    } else {
        let mut terminal_operands: Vec<IdentifierId> = Vec::new();
        for_each_terminal_operand(&block.terminal, |operand| {
            terminal_operands.push(operand.identifier.id);
        });
        for operand in terminal_operands {
            emit(&mut locals, shared, &mut next_instructions, operand);
        }

        let ids_reversed: Vec<IdentifierId> = locals.keys().copied().rev().collect();
        for id in ids_reversed {
            let Some(node) = locals.get(&id).cloned() else {
                continue;
            };
            if matches!(node.reorderability, Reorderability::Reorderable) {
                shared.insert(id, node);
            } else {
                emit(&mut locals, shared, &mut next_instructions, id);
            }
        }
    }

    block.instructions = next_instructions;
}

fn get_depth(nodes: &mut Nodes, id: IdentifierId) -> usize {
    let Some(existing) = nodes.get(&id) else {
        return 0;
    };
    if let Some(depth) = existing.depth {
        return depth;
    }

    let (deps, reorderability) = {
        let node = nodes
            .get_mut(&id)
            .expect("node disappeared during depth calc");
        node.depth = Some(0); // break potential cycles
        (
            node.dependencies.iter().copied().collect::<Vec<_>>(),
            node.reorderability,
        )
    };

    let mut depth = if matches!(reorderability, Reorderability::Reorderable) {
        1usize
    } else {
        10usize
    };
    for dep in deps {
        depth += get_depth(nodes, dep);
    }

    if let Some(node) = nodes.get_mut(&id) {
        node.depth = Some(depth);
    }
    depth
}

fn emit(
    locals: &mut Nodes,
    shared: &mut Nodes,
    instructions: &mut Vec<Instruction>,
    id: IdentifierId,
) {
    let Some(node) = locals
        .shift_remove(&id)
        .or_else(|| shared.shift_remove(&id))
    else {
        return;
    };

    let mut deps: Vec<IdentifierId> = node.dependencies.iter().copied().collect();
    deps.sort_by(|a, b| {
        let a_depth = get_depth(locals, *a);
        let b_depth = get_depth(locals, *b);
        b_depth.cmp(&a_depth)
    });

    for dep in deps {
        emit(locals, shared, instructions, dep);
    }

    if let Some(instr) = node.instruction {
        instructions.push(instr);
    }
}

fn for_each_instruction_value_lvalue(value: &InstructionValue, mut f: impl FnMut(&Place)) {
    match value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            f(&lvalue.place);
        }
        InstructionValue::Destructure { lvalue, .. } => {
            for_each_pattern_place(&lvalue.pattern, &mut f);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            f(lvalue);
        }
        _ => {}
    }
}

fn get_reorderability(instr: &Instruction, references: &References) -> Reorderability {
    match &instr.value {
        InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::UnaryExpression { .. } => Reorderability::Reorderable,
        InstructionValue::LoadLocal { place, .. } => {
            if let Some(name) = named_identifier(&place.identifier.name)
                && let Some(last_assignment) = references.last_assignments.get(name)
                && last_assignment.0 < instr.id.0
                && references
                    .single_use_identifiers
                    .contains(&instr.lvalue.identifier.id)
            {
                return Reorderability::Reorderable;
            }
            Reorderability::Nonreorderable
        }
        _ => Reorderability::Nonreorderable,
    }
}
