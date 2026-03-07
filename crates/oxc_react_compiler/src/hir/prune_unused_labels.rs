//! Port of PruneUnusedLabelsHIR.ts.
//!
//! Removes Label terminals whose body block immediately breaks to the label's
//! fallthrough. In such cases the label is dead code: no instruction actually
//! needs the label for a `break` target, so the label+body+fallthrough blocks
//! can be merged into the label block.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use super::types::*;

/// Information about a label terminal that can be pruned.
struct MergeCandidate {
    /// The block id containing the Label terminal.
    label: BlockId,
    /// The block id of the Label's `block` (the body).
    next: BlockId,
    /// The block id of the Label's `fallthrough`.
    fallthrough: BlockId,
}

/// Prune label terminals that have no useful break target.
///
/// A Label terminal is unused when its body block (`next`) ends with a
/// `Goto { variant: Break }` that targets the label's `fallthrough`, meaning
/// nothing actually needs the label -- the body just falls through. In that
/// case we merge the body's instructions and fallthrough's instructions into
/// the label block, remove the intermediate blocks, and replace the label
/// terminal with the fallthrough's terminal.
pub fn prune_unused_labels_hir(func: &mut HIRFunction) {
    // Build a lookup from BlockId -> index for O(1) access.
    let block_index: HashMap<BlockId, usize> = func
        .body
        .blocks
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (*id, i))
        .collect();

    // Phase 1: Identify merge candidates.
    let mut merged: Vec<MergeCandidate> = Vec::new();

    for (_block_id, block) in &func.body.blocks {
        if let Terminal::Label {
            block: next_id,
            fallthrough: fallthrough_id,
            ..
        } = &block.terminal
        {
            // Look up the next (body) block
            if let Some(&next_idx) = block_index.get(next_id) {
                let (_, next_block) = &func.body.blocks[next_idx];

                // Check if the body ends with a Goto Break to the fallthrough
                if let Terminal::Goto {
                    block: goto_target,
                    variant: GotoVariant::Break,
                    ..
                } = &next_block.terminal
                    && goto_target == fallthrough_id
                {
                    // Check if the fallthrough block exists
                    if let Some(&ft_idx) = block_index.get(fallthrough_id) {
                        let (_, ft_block) = &func.body.blocks[ft_idx];

                        // Only merge normal block types (upstream: block.kind === 'block')
                        if next_block.kind == BlockKind::Block && ft_block.kind == BlockKind::Block
                        {
                            merged.push(MergeCandidate {
                                label: block.id,
                                next: *next_id,
                                fallthrough: *fallthrough_id,
                            });
                        }
                    }
                }
            }
        }
    }

    if merged.is_empty() {
        return;
    }

    // Phase 2: Perform merges.
    // Track rewrites for blocks that were merged (fallthrough -> label).
    let mut rewrites: HashMap<BlockId, BlockId> = HashMap::new();
    let mut blocks_to_remove: HashSet<BlockId> = HashSet::new();

    for candidate in &merged {
        // The label block id might have been rewritten by a previous merge.
        let label_id = *rewrites.get(&candidate.label).unwrap_or(&candidate.label);
        let next_id = candidate.next;
        let fallthrough_id = candidate.fallthrough;

        // Find the label block by id.
        let label_idx = func.body.blocks.iter().position(|(_, b)| b.id == label_id);
        let label_idx = match label_idx {
            Some(idx) => idx,
            None => continue,
        };

        let next_idx = func.body.blocks.iter().position(|(_, b)| b.id == next_id);
        let next_idx = match next_idx {
            Some(idx) => idx,
            None => continue,
        };

        let ft_idx = func
            .body
            .blocks
            .iter()
            .position(|(_, b)| b.id == fallthrough_id);
        let ft_idx = match ft_idx {
            Some(idx) => idx,
            None => continue,
        };

        // Upstream invariant: next and fallthrough should have empty phis.
        assert!(
            func.body.blocks[next_idx].1.phis.is_empty()
                && func.body.blocks[ft_idx].1.phis.is_empty(),
            "Unexpected phis when merging label blocks"
        );

        // Upstream invariant: next should have exactly 1 pred (the original label),
        // and fallthrough should have exactly 1 pred (next).
        assert!(
            func.body.blocks[next_idx].1.preds.len() == 1
                && func.body.blocks[ft_idx].1.preds.len() == 1
                && func.body.blocks[next_idx]
                    .1
                    .preds
                    .contains(&candidate.label)
                && func.body.blocks[ft_idx].1.preds.contains(&next_id),
            "Unexpected block predecessors when merging label blocks"
        );

        // Move instructions from next and fallthrough into the label block.
        // We need to collect them first to avoid borrow issues.
        let next_instructions: Vec<Instruction> = func.body.blocks[next_idx]
            .1
            .instructions
            .drain(..)
            .collect();
        let ft_instructions: Vec<Instruction> =
            func.body.blocks[ft_idx].1.instructions.drain(..).collect();

        // Replace the label block's terminal with the fallthrough's terminal.
        let ft_terminal = std::mem::replace(
            &mut func.body.blocks[ft_idx].1.terminal,
            Terminal::Unreachable {
                id: InstructionId::default(),
                loc: SourceLocation::Generated,
            },
        );

        func.body.blocks[label_idx]
            .1
            .instructions
            .extend(next_instructions);
        func.body.blocks[label_idx]
            .1
            .instructions
            .extend(ft_instructions);
        func.body.blocks[label_idx].1.terminal = ft_terminal;

        // Mark next and fallthrough for removal.
        blocks_to_remove.insert(next_id);
        blocks_to_remove.insert(fallthrough_id);

        // Record rewrite: fallthrough -> label (for transitive resolution)
        rewrites.insert(fallthrough_id, label_id);
    }

    // Remove merged blocks.
    func.body
        .blocks
        .retain(|(_, block)| !blocks_to_remove.contains(&block.id));

    // Phase 3: Rewrite predecessor sets.
    for (_block_id, block) in &mut func.body.blocks {
        let preds_to_rewrite: Vec<(BlockId, BlockId)> = block
            .preds
            .iter()
            .filter_map(|pred| rewrites.get(pred).map(|&new| (*pred, new)))
            .collect();
        for (old, new) in preds_to_rewrite {
            block.preds.remove(&old);
            block.preds.insert(new);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_place(ident_id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(ident_id),
                declaration_id: DeclarationId(ident_id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_func(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_prune_unused_label() {
        // Block 0: Label { block: 1, fallthrough: 2 }
        // Block 1: [instr] Goto Break -> 2
        // Block 2: [instr] Return
        //
        // After pruning, block 0 should absorb blocks 1 and 2,
        // and blocks 1 and 2 should be removed.
        let preds_0 = HashSet::new();
        let mut preds_1 = HashSet::new();
        preds_1.insert(BlockId(0));
        let mut preds_2 = HashSet::new();
        preds_2.insert(BlockId(1));

        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![Instruction {
                        id: InstructionId(1),
                        lvalue: make_place(1),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Undefined,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Label {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        id: InstructionId(2),
                        loc: SourceLocation::Generated,
                    },
                    preds: preds_0,
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![Instruction {
                        id: InstructionId(3),
                        lvalue: make_place(2),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Null,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: preds_1,
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![Instruction {
                        id: InstructionId(5),
                        lvalue: make_place(3),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Boolean(true),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    }],
                    terminal: Terminal::Return {
                        value: make_place(3),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(6),
                        loc: SourceLocation::Generated,
                    },
                    preds: preds_2,
                    phis: vec![],
                },
            ),
        ]);

        prune_unused_labels_hir(&mut func);

        // Should only have 1 block remaining (block 0)
        assert_eq!(func.body.blocks.len(), 1);
        assert_eq!(func.body.blocks[0].0, BlockId(0));

        // Block 0 should now have 3 instructions (1 original + 1 from block 1 + 1 from block 2)
        assert_eq!(func.body.blocks[0].1.instructions.len(), 3);

        // Terminal should be the Return from block 2
        assert!(matches!(
            func.body.blocks[0].1.terminal,
            Terminal::Return { .. }
        ));
    }

    #[test]
    fn test_no_labels_is_noop() {
        let mut func = make_func(vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![Instruction {
                    id: InstructionId(1),
                    lvalue: make_place(1),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                }],
                terminal: Terminal::Return {
                    value: make_place(1),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )]);

        prune_unused_labels_hir(&mut func);

        // Should still have 1 block
        assert_eq!(func.body.blocks.len(), 1);
    }

    #[test]
    fn test_label_with_actual_break_not_pruned() {
        // Block 0: Label { block: 1, fallthrough: 2 }
        // Block 1: If { ... } -> break to 2 or continue
        // This should NOT be pruned because block 1 doesn't end with a simple Goto Break.
        let mut preds_1 = HashSet::new();
        preds_1.insert(BlockId(0));
        let mut preds_2 = HashSet::new();
        preds_2.insert(BlockId(1));

        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Label {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![],
                    terminal: Terminal::If {
                        test: make_place(1),
                        consequent: BlockId(3),
                        alternate: BlockId(4),
                        fallthrough: BlockId(2),
                        id: InstructionId(2),
                        loc: SourceLocation::Generated,
                    },
                    preds: preds_1,
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(3),
                        loc: SourceLocation::Generated,
                    },
                    preds: preds_2,
                    phis: vec![],
                },
            ),
            (
                BlockId(3),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(3),
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(4),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(4),
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(5),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        prune_unused_labels_hir(&mut func);

        // All 5 blocks should remain because the label body has an If terminal, not Goto Break
        assert_eq!(func.body.blocks.len(), 5);
    }
}
