//! MergeConsecutiveBlocks — merges blocks that always execute consecutively.
//!
//! Port of `HIR/MergeConsecutiveBlocks.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Merges sequences of blocks that will always execute consecutively —
//! i.e. where the predecessor always transfers control to the successor
//! (ends in a Goto) and where the predecessor is the only predecessor
//! for that successor (there is no other way to reach the successor).
//!
//! Value/loop blocks are left alone because they cannot be merged without
//! breaking the structure of the high-level terminals that reference them.

use std::collections::{HashMap, HashSet};

use super::prune_maybe_throws::mark_predecessors;
use super::types::*;

/// Tracks which blocks have been merged into other blocks.
/// Supports transitive resolution (A merged into B, B merged into C → A maps to C).
struct MergedBlocks {
    map: HashMap<BlockId, BlockId>,
}

impl MergedBlocks {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Record that `block` was merged into `into`.
    fn merge(&mut self, block: BlockId, into: BlockId) {
        let target = self.get(into);
        self.map.insert(block, target);
    }

    /// Get the id of the block that `block` has been merged into.
    /// This is transitive: if A was merged into B which merged into C,
    /// `get(A)` returns C.
    fn get(&self, block: BlockId) -> BlockId {
        let mut current = block;
        while let Some(&next) = self.map.get(&current) {
            current = next;
        }
        current
    }
}

/// Get the terminal's fallthrough block id, if it has one.
/// This corresponds to `terminalFallthrough` in the upstream.
fn terminal_fallthrough(terminal: &Terminal) -> Option<BlockId> {
    terminal.fallthrough()
}

/// Set the terminal's fallthrough block id to a new value.
fn set_terminal_fallthrough(terminal: &mut Terminal, new_fallthrough: BlockId) {
    match terminal {
        Terminal::If { fallthrough, .. }
        | Terminal::Branch { fallthrough, .. }
        | Terminal::Switch { fallthrough, .. }
        | Terminal::For { fallthrough, .. }
        | Terminal::ForOf { fallthrough, .. }
        | Terminal::ForIn { fallthrough, .. }
        | Terminal::DoWhile { fallthrough, .. }
        | Terminal::While { fallthrough, .. }
        | Terminal::Logical { fallthrough, .. }
        | Terminal::Ternary { fallthrough, .. }
        | Terminal::Optional { fallthrough, .. }
        | Terminal::Label { fallthrough, .. }
        | Terminal::Sequence { fallthrough, .. }
        | Terminal::Try { fallthrough, .. }
        | Terminal::Scope { fallthrough, .. }
        | Terminal::PrunedScope { fallthrough, .. } => {
            *fallthrough = new_fallthrough;
        }
        // These terminals have no fallthrough
        Terminal::Unsupported { .. }
        | Terminal::Unreachable { .. }
        | Terminal::Throw { .. }
        | Terminal::Return { .. }
        | Terminal::Goto { .. } => {}
    }
}

/// Merge consecutive blocks in the HIR function.
///
/// Also recurses into nested `FunctionExpression` and `ObjectMethod` instruction values.
pub fn merge_consecutive_blocks(func: &mut HIRFunction) {
    let mut merged = MergedBlocks::new();

    // Collect fallthrough blocks first (blocks that are the fallthrough target of some terminal).
    let mut fallthrough_blocks: HashSet<BlockId> = HashSet::new();

    // We'll iterate by index so we can look up predecessors while mutating.
    // First pass: collect fallthroughs and recurse into nested functions.
    for i in 0..func.body.blocks.len() {
        let (_, block) = &func.body.blocks[i];
        if let Some(ft) = terminal_fallthrough(&block.terminal) {
            fallthrough_blocks.insert(ft);
        }

        // Recurse into nested functions
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { .. }
                | InstructionValue::ObjectMethod { .. } => {
                    // We need mutable access. We'll do this in a second pass below.
                }
                _ => {}
            }
        }
    }

    // Recurse into nested functions (requires mutable access).
    for i in 0..func.body.blocks.len() {
        let (_, block) = &mut func.body.blocks[i];
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    merge_consecutive_blocks(&mut lowered_func.func);
                }
                _ => {}
            }
        }
    }

    // Track which block IDs to remove after merging.
    let mut blocks_to_remove: HashSet<BlockId> = HashSet::new();

    // Main merge loop: iterate over blocks and merge where possible.
    // We need to be careful because we're modifying blocks while iterating.
    // Strategy: iterate by index, and for each candidate block, find its predecessor
    // and merge if the conditions are met.
    let block_count = func.body.blocks.len();
    for i in 0..block_count {
        let (_, block) = &func.body.blocks[i];
        let block_id = block.id;

        // Skip already-removed blocks
        if blocks_to_remove.contains(&block_id) {
            continue;
        }

        // Can only merge blocks with a single predecessor
        if block.preds.len() != 1 {
            continue;
        }

        // Value blocks cannot merge
        if block.kind != BlockKind::Block {
            continue;
        }

        // Merging across fallthroughs could move the predecessor out of its block scope
        if fallthrough_blocks.contains(&block_id) {
            continue;
        }

        let original_predecessor_id = *block.preds.iter().next().unwrap();
        let predecessor_id = merged.get(original_predecessor_id);

        // Find the predecessor block
        let pred_idx = func
            .body
            .blocks
            .iter()
            .position(|(_, b)| b.id == predecessor_id);
        let pred_idx = match pred_idx {
            Some(idx) => idx,
            None => continue,
        };

        // Check that the predecessor has a Goto terminal and is a 'block' kind
        let pred_terminal_is_goto = matches!(
            &func.body.blocks[pred_idx].1.terminal,
            Terminal::Goto { .. }
        );
        let pred_is_block = func.body.blocks[pred_idx].1.kind == BlockKind::Block;

        if !pred_terminal_is_goto || !pred_is_block {
            continue;
        }

        // Get the terminal id from the predecessor for the phi → LoadLocal instructions
        let pred_terminal_id = func.body.blocks[pred_idx].1.terminal.id();

        // Replace phis in the merged block with LoadLocal instructions appended to predecessor
        let phis: Vec<Phi> = func.body.blocks[i].1.phis.drain(..).collect();
        for phi in phis {
            assert!(
                phi.operands.len() == 1,
                "Found a block with a single predecessor but where a phi has {} operands",
                phi.operands.len()
            );
            let operand = phi.operands.into_values().next().unwrap();
            let lvalue = Place {
                identifier: phi.place.identifier,
                effect: Effect::ConditionallyMutate,
                reactive: false,
                loc: SourceLocation::Generated,
            };
            let instr = Instruction {
                id: pred_terminal_id,
                lvalue: lvalue.clone(),
                value: InstructionValue::LoadLocal {
                    place: operand,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            };
            func.body.blocks[pred_idx].1.instructions.push(instr);
        }

        // Move instructions from the merged block to the predecessor
        let mut instructions: Vec<Instruction> =
            func.body.blocks[i].1.instructions.drain(..).collect();
        func.body.blocks[pred_idx]
            .1
            .instructions
            .append(&mut instructions);

        // Replace predecessor's terminal with the merged block's terminal
        let new_terminal = std::mem::replace(
            &mut func.body.blocks[i].1.terminal,
            Terminal::Unreachable {
                id: InstructionId::default(),
                loc: SourceLocation::Generated,
            },
        );
        func.body.blocks[pred_idx].1.terminal = new_terminal;

        // Record the merge
        merged.merge(block_id, predecessor_id);
        blocks_to_remove.insert(block_id);
    }

    // Remove merged blocks
    func.body
        .blocks
        .retain(|(_, block)| !blocks_to_remove.contains(&block.id));

    // Update phi operands that reference merged blocks
    for (_, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            let updates: Vec<(BlockId, BlockId)> = phi
                .operands
                .keys()
                .filter_map(|&pred_id| {
                    let mapped = merged.get(pred_id);
                    if mapped != pred_id {
                        Some((pred_id, mapped))
                    } else {
                        None
                    }
                })
                .collect();
            for (old, new) in updates {
                if let Some(operand) = phi.operands.remove(&old) {
                    phi.operands.insert(new, operand);
                }
            }
        }
    }

    // Recompute predecessors
    mark_predecessors(&mut func.body);

    // Update fallthrough references in terminals
    for (_, block) in &mut func.body.blocks {
        if terminal_fallthrough(&block.terminal).is_some() {
            let ft = terminal_fallthrough(&block.terminal).unwrap();
            let mapped = merged.get(ft);
            if mapped != ft {
                set_terminal_fallthrough(&mut block.terminal, mapped);
            }
        }
    }
}
