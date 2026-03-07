//! Remove unnecessary try-catch blocks.
//!
//! Port of `removeUnnecessaryTryCatch` from upstream HIRBuilder.ts.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! In the upstream, each instruction inside a try block gets a MaybeThrow terminal.
//! When all MaybeThrow terminals are pruned (because all instructions are non-throwing),
//! the handler becomes unreachable and is removed by reversePostorderBlocks.
//! removeUnnecessaryTryCatch then converts the Try terminal to Goto.
//!
//! Our HIR builder does not yet generate MaybeThrow terminals for every instruction
//! inside try blocks. So instead of relying on MaybeThrow pruning, we directly check
//! whether the try body contains any potentially throwing instructions. If the entire
//! try body (all blocks reachable from the try block up to the fallthrough) has only
//! non-throwing instructions, we remove the handler and convert Try to Goto.

use std::collections::{HashSet, VecDeque};

use crate::hir::builder::terminal_successors;
use crate::hir::types::*;

/// Check if a single instruction may throw.
/// Only Primitive, ArrayExpression, and ObjectExpression are known non-throwing.
/// This matches the upstream `instructionMayThrow` in PruneMaybeThrows.ts.
fn instruction_may_throw(instr: &Instruction) -> bool {
    !matches!(
        &instr.value,
        InstructionValue::Primitive { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
    )
}

/// Collect all blocks reachable from `start` within the try body.
/// Stops at the fallthrough block (does not include it).
/// Uses full terminal_successors to walk the body.
fn collect_try_body_blocks(body: &HIR, start: BlockId, fallthrough: BlockId) -> HashSet<BlockId> {
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut queue: VecDeque<BlockId> = VecDeque::new();
    queue.push_back(start);
    visited.insert(start);

    while let Some(block_id) = queue.pop_front() {
        // Don't follow past the fallthrough
        if block_id == fallthrough {
            continue;
        }
        let terminal = body
            .blocks
            .iter()
            .find(|(id, _)| *id == block_id)
            .map(|(_, b)| &b.terminal);
        if let Some(terminal) = terminal {
            for succ in terminal_successors(terminal) {
                if succ != fallthrough && visited.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }

    visited
}

/// Check if any block in the try body contains throwing instructions.
fn try_body_may_throw(body: &HIR, try_block: BlockId, fallthrough: BlockId) -> bool {
    let body_blocks = collect_try_body_blocks(body, try_block, fallthrough);

    for (_, block) in &body.blocks {
        if !body_blocks.contains(&block.id) {
            continue;
        }
        for instr in &block.instructions {
            if instruction_may_throw(instr) {
                return true;
            }
        }
    }

    false
}

/// Combined pass: remove unnecessary try-catch blocks and clean up.
///
/// For each Try terminal, checks if the try body contains any throwing
/// instructions. If not, the handler is unnecessary and can be removed.
///
/// Also recomputes predecessors and instruction IDs afterward.
pub fn cleanup_after_terminal_changes(func: &mut HIRFunction) {
    let debug = std::env::var("DEBUG_TRY_CLEANUP").is_ok();

    // Check if there are any Try terminals at all -- early exit if not
    if debug {
        let ids: Vec<u32> = func.body.blocks.iter().map(|(id, _)| id.0).collect();
        eprintln!("[TRY_CLEANUP] func blocks={ids:?}");
        for (id, block) in &func.body.blocks {
            if let Terminal::Try {
                block: try_block,
                handler,
                fallthrough,
                ..
            } = &block.terminal
            {
                eprintln!(
                    "[TRY_CLEANUP]   bb{} try block=bb{} handler=bb{} ft=bb{} kind={:?}",
                    id.0, try_block.0, handler.0, fallthrough.0, block.kind
                );
            }
        }
    }

    let has_try = func
        .body
        .blocks
        .iter()
        .any(|(_, block)| matches!(&block.terminal, Terminal::Try { .. }));
    if !has_try {
        return;
    }

    // Find Try terminals to convert:
    // - try bodies that cannot throw
    // - tries whose handler block is already missing
    let existing_block_ids: HashSet<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();

    let try_conversions: Vec<(
        BlockId,
        BlockId,
        BlockId,
        BlockId,
        InstructionId,
        SourceLocation,
    )> = func
        .body
        .blocks
        .iter()
        .filter_map(|(_, block)| {
            if let Terminal::Try {
                block: try_block,
                handler,
                fallthrough,
                id,
                loc,
                ..
            } = &block.terminal
            {
                let missing_handler = !existing_block_ids.contains(handler);
                let may_throw = try_body_may_throw(&func.body, *try_block, *fallthrough);
                if debug {
                    eprintln!(
                        "[TRY_CLEANUP] try parent=bb{} try=bb{} handler=bb{} ft=bb{} missing_handler={} may_throw={}",
                        block.id.0, try_block.0, handler.0, fallthrough.0, missing_handler, may_throw
                    );
                }
                if missing_handler || !may_throw {
                    Some((
                        block.id,
                        *try_block,
                        *handler,
                        *fallthrough,
                        *id,
                        loc.clone(),
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();

    if try_conversions.is_empty() {
        if debug {
            eprintln!("[TRY_CLEANUP] no conversions");
        }
        return;
    }

    if debug {
        eprintln!(
            "[TRY_CLEANUP] converting {} try terminal(s)",
            try_conversions.len()
        );
    }

    // Build reachability set from entry, excluding handler edges of Try terminals
    // that we are about to convert.
    let converting_handlers: HashSet<BlockId> = try_conversions
        .iter()
        .map(|&(_, _, handler_id, _, _, _)| handler_id)
        .collect();

    let entry = func.body.entry;
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut queue: VecDeque<BlockId> = VecDeque::new();
    queue.push_back(entry);
    reachable.insert(entry);

    while let Some(block_id) = queue.pop_front() {
        let terminal = func
            .body
            .blocks
            .iter()
            .find(|(id, _)| *id == block_id)
            .map(|(_, b)| &b.terminal);
        if let Some(terminal) = terminal {
            let mut succs = terminal_successors(terminal);
            // For Try terminals we are converting, skip the handler successor
            if let Terminal::Try { handler, .. } = terminal
                && converting_handlers.contains(handler)
            {
                succs.retain(|s| s != handler);
            }
            for succ in succs {
                if reachable.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }

    // Remove unreachable blocks (handler blocks and their descendants)
    let before = func.body.blocks.len();
    func.body.blocks.retain(|(id, _)| reachable.contains(id));
    let _removed_unreachable = func.body.blocks.len() != before;

    // Convert Try terminals to Goto where handler was removed
    let existing_blocks: HashSet<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();

    for (parent_id, try_block, handler_id, _fallthrough_id, term_id, term_loc) in &try_conversions {
        if !existing_blocks.contains(handler_id)
            && let Some((_, block)) = func
                .body
                .blocks
                .iter_mut()
                .find(|(_, b)| b.id == *parent_id)
        {
            block.terminal = Terminal::Goto {
                block: *try_block,
                variant: GotoVariant::Break,
                id: *term_id,
                loc: term_loc.clone(),
            };
        }
    }

    // After converting Try to Goto, remove newly unreachable blocks
    let mut reachable2: HashSet<BlockId> = HashSet::new();
    let mut queue2: VecDeque<BlockId> = VecDeque::new();
    queue2.push_back(entry);
    reachable2.insert(entry);
    while let Some(block_id) = queue2.pop_front() {
        let terminal = func
            .body
            .blocks
            .iter()
            .find(|(id, _)| *id == block_id)
            .map(|(_, b)| &b.terminal);
        if let Some(terminal) = terminal {
            for succ in terminal_successors(terminal) {
                if reachable2.insert(succ) {
                    queue2.push_back(succ);
                }
            }
        }
    }
    func.body.blocks.retain(|(id, _)| reachable2.contains(id));

    // Recompute predecessors using full terminal_successors
    for (_block_id, block) in &mut func.body.blocks {
        block.preds.clear();
    }
    let edges: Vec<(BlockId, BlockId)> = func
        .body
        .blocks
        .iter()
        .flat_map(|(_, block)| {
            let src = block.id;
            terminal_successors(&block.terminal)
                .into_iter()
                .map(move |tgt| (src, tgt))
        })
        .collect();
    for (src, tgt) in edges {
        for (_block_id, block) in &mut func.body.blocks {
            if block.id == tgt {
                block.preds.insert(src);
            }
        }
    }

    // Re-number instruction IDs
    let mut next_id = 0u32;
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            next_id += 1;
            instr.id = InstructionId::new(next_id);
        }
        next_id += 1;
        assign_terminal_id(&mut block.terminal, InstructionId::new(next_id));
    }
}

fn assign_terminal_id(terminal: &mut Terminal, id: InstructionId) {
    match terminal {
        Terminal::Unsupported { id: tid, .. }
        | Terminal::Unreachable { id: tid, .. }
        | Terminal::Throw { id: tid, .. }
        | Terminal::Return { id: tid, .. }
        | Terminal::Goto { id: tid, .. }
        | Terminal::If { id: tid, .. }
        | Terminal::Branch { id: tid, .. }
        | Terminal::Switch { id: tid, .. }
        | Terminal::For { id: tid, .. }
        | Terminal::ForOf { id: tid, .. }
        | Terminal::ForIn { id: tid, .. }
        | Terminal::DoWhile { id: tid, .. }
        | Terminal::While { id: tid, .. }
        | Terminal::Logical { id: tid, .. }
        | Terminal::Ternary { id: tid, .. }
        | Terminal::Optional { id: tid, .. }
        | Terminal::Label { id: tid, .. }
        | Terminal::Sequence { id: tid, .. }
        | Terminal::Try { id: tid, .. }
        | Terminal::MaybeThrow { id: tid, .. }
        | Terminal::Scope { id: tid, .. }
        | Terminal::PrunedScope { id: tid, .. } => *tid = id,
    }
}
