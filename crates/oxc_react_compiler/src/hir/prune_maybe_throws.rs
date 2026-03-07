//! PruneMaybeThrows — removes MaybeThrow terminals from non-throwing blocks.
//!
//! Port of `Optimization/PruneMaybeThrows.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass prunes `MaybeThrow` terminals for blocks that can provably *never* throw.
//! For now this is very conservative and only affects blocks with primitives or
//! array/object literals. Even a variable reference could throw because of the TDZ.
//!
//! After pruning, the pass rewrites phi operands whose predecessors were removed
//! by the terminal changes.

use std::collections::HashMap;

use super::builder::{each_terminal_successor, terminal_successors};
use super::types::*;

/// Check if a single instruction may throw.
/// Only Primitive, ArrayExpression, and ObjectExpression are known non-throwing.
fn instruction_may_throw(instr: &Instruction) -> bool {
    !matches!(
        &instr.value,
        InstructionValue::Primitive { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
    )
}

/// Core implementation: walk all blocks, replace MaybeThrow terminals with Goto
/// when the block's instructions are all non-throwing.
///
/// Returns a mapping from removed continuation blocks to the source block that
/// now skips them, or `None` if no changes were made.
fn prune_maybe_throws_impl(func: &mut HIRFunction) -> Option<HashMap<BlockId, BlockId>> {
    let mut terminal_mapping: HashMap<BlockId, BlockId> = HashMap::new();

    for (_block_id, block) in &mut func.body.blocks {
        let (continuation, term_id, term_loc) = match &block.terminal {
            Terminal::MaybeThrow {
                continuation,
                id,
                loc,
                ..
            } => (*continuation, *id, loc.clone()),
            _ => continue,
        };

        let can_throw = block.instructions.iter().any(instruction_may_throw);

        if !can_throw {
            let source = terminal_mapping.get(&block.id).copied().unwrap_or(block.id);
            terminal_mapping.insert(continuation, source);
            block.terminal = Terminal::Goto {
                block: continuation,
                variant: GotoVariant::Break,
                id: term_id,
                loc: term_loc,
            };
        }
    }

    if terminal_mapping.is_empty() {
        None
    } else {
        Some(terminal_mapping)
    }
}

/// Recompute predecessor sets for all blocks in the HIR body.
pub fn mark_predecessors(body: &mut HIR) {
    // Clear all preds first
    for (_block_id, block) in &mut body.blocks {
        block.preds.clear();
    }

    // Collect (source, target) pairs
    let edges: Vec<(BlockId, BlockId)> = body
        .blocks
        .iter()
        .flat_map(|(_, block)| {
            let src = block.id;
            each_terminal_successor(&block.terminal)
                .into_iter()
                .map(move |tgt| (src, tgt))
        })
        .collect();

    // Insert preds
    for (src, tgt) in edges {
        for (_block_id, block) in &mut body.blocks {
            if block.id == tgt {
                block.preds.insert(src);
            }
        }
    }
}

/// Re-assign sequential instruction IDs across all blocks.
pub fn mark_instruction_ids(body: &mut HIR) {
    let mut next_id = 0u32;
    for (_block_id, block) in &mut body.blocks {
        for instr in &mut block.instructions {
            next_id += 1;
            instr.id = InstructionId::new(next_id);
        }
        next_id += 1;
        // Assign the terminal its ID
        assign_terminal_id_value(&mut block.terminal, InstructionId::new(next_id));
    }
}

fn assign_terminal_id_value(terminal: &mut Terminal, id: InstructionId) {
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

/// Check if any instruction in blocks reachable from `start_block` (within the
/// try body, i.e., not crossing into `handler` or `fallthrough`) can throw.
fn try_body_can_throw(
    body: &HIR,
    start_block: BlockId,
    handler: BlockId,
    fallthrough: BlockId,
) -> bool {
    use std::collections::HashSet;

    let block_map: HashMap<BlockId, &BasicBlock> =
        body.blocks.iter().map(|(id, b)| (*id, b)).collect();

    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut queue: std::collections::VecDeque<BlockId> = std::collections::VecDeque::new();
    queue.push_back(start_block);
    visited.insert(start_block);
    // Don't traverse into handler or fallthrough — those are outside the try body
    visited.insert(handler);
    visited.insert(fallthrough);

    while let Some(block_id) = queue.pop_front() {
        if let Some(block) = block_map.get(&block_id) {
            // Check if any instruction in this block can throw
            if block.instructions.iter().any(instruction_may_throw) {
                return true;
            }
            // Visit successors (within the try body)
            for succ in terminal_successors(&block.terminal) {
                if visited.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }

    false
}

/// Remove Try terminals whose try body cannot throw.
///
/// When the try body has no instructions that can throw, the catch handler
/// is unreachable and the Try terminal can be simplified to a Goto to the
/// try body block.
///
/// This also removes the now-dead handler blocks.
///
/// Returns `true` if any Try terminals were removed.
///
/// Port of `removeUnnecessaryTryCatch` from upstream `HIRBuilder.ts`.
pub fn remove_unnecessary_try_catch(body: &mut HIR) -> bool {
    use std::collections::HashSet;

    // Collect Try terminals that can be simplified
    // (block_with_try_id, try_block, handler, fallthrough, term_id, term_loc)
    let mut try_to_simplify: Vec<(
        BlockId,
        BlockId,
        BlockId,
        BlockId,
        InstructionId,
        SourceLocation,
    )> = Vec::new();

    for (_, block) in &body.blocks {
        if let Terminal::Try {
            block: try_block,
            handler,
            fallthrough,
            id,
            loc,
            ..
        } = &block.terminal
            && !try_body_can_throw(body, *try_block, *handler, *fallthrough)
        {
            try_to_simplify.push((
                block.id,
                *try_block,
                *handler,
                *fallthrough,
                *id,
                loc.clone(),
            ));
        }
    }

    if try_to_simplify.is_empty() {
        return false;
    }

    let mut handler_blocks_to_remove: HashSet<BlockId> = HashSet::new();

    for (block_id, try_block, handler_id, _fallthrough_id, term_id, term_loc) in &try_to_simplify {
        handler_blocks_to_remove.insert(*handler_id);

        // Convert Try terminal to Goto
        if let Some((_, block)) = body.blocks.iter_mut().find(|(id, _)| id == block_id) {
            block.terminal = Terminal::Goto {
                block: *try_block,
                variant: GotoVariant::Break,
                id: *term_id,
                loc: term_loc.clone(),
            };
        }
    }

    // Remove handler blocks
    body.blocks
        .retain(|(id, _)| !handler_blocks_to_remove.contains(id));

    // Also remove blocks that are now unreachable (transitively from handler)
    let mut reachable: HashSet<BlockId> = HashSet::new();
    let mut queue: std::collections::VecDeque<BlockId> = std::collections::VecDeque::new();
    queue.push_back(body.entry);
    reachable.insert(body.entry);
    while let Some(block_id) = queue.pop_front() {
        if let Some((_, block)) = body.blocks.iter().find(|(id, _)| *id == block_id) {
            for succ in terminal_successors(&block.terminal) {
                if reachable.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }
    body.blocks.retain(|(id, _)| reachable.contains(id));
    true
}

/// After removing try-catch blocks, some StoreLocal instructions may become
/// dead stores (their SSA ID is no longer read). Rewrite these to DeclareLocal
/// to keep the variable declaration but remove the dead initializer, then
/// remove orphaned instructions that only existed to produce the initializer.
///
/// This mirrors upstream's behavior where DCE's `rewriteInstruction` converts
/// StoreLocal→DeclareLocal while the variable name is still alive, and then
/// the DeclareLocal persists after try-catch removal because no subsequent
/// DCE pass runs.
fn rewrite_dead_stores_after_try_catch_removal(func: &mut HIRFunction) {
    use std::collections::HashSet;

    // Phase 1: Collect all identifier IDs that are actually used (read)
    // across the remaining blocks.
    let mut used_ids: HashSet<IdentifierId> = HashSet::new();

    for (_, block) in &func.body.blocks {
        // Terminal operands
        collect_terminal_used_ids(&block.terminal, &mut used_ids);

        // Instruction operands (what each instruction reads)
        for instr in &block.instructions {
            super::visitors::for_each_instruction_operand(instr, |place| {
                used_ids.insert(place.identifier.id);
            });
        }

        // Phi operands
        for phi in &block.phis {
            for operand in phi.operands.values() {
                used_ids.insert(operand.identifier.id);
            }
        }
    }

    // Phase 2: Rewrite StoreLocal → DeclareLocal for dead stores.
    // A StoreLocal is a "dead store" if:
    // 1. It's not a Reassign (it's a Let/Const declaration)
    // 2. The lvalue's SSA ID is not in used_ids (the specific version is never read)
    let mut rhs_ids_to_remove: HashSet<IdentifierId> = HashSet::new();

    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                && lvalue.kind != InstructionKind::Reassign
                && !used_ids.contains(&lvalue.place.identifier.id)
            {
                // Track the RHS temp so we can remove the instruction that
                // produced it (it's now orphaned).
                rhs_ids_to_remove.insert(value.identifier.id);
                let new_lvalue = lvalue.clone();
                let loc = instr.value.loc().clone();
                instr.value = InstructionValue::DeclareLocal {
                    lvalue: new_lvalue,
                    loc,
                };
            }
        }
    }

    // Phase 3: Remove instructions that produced the now-orphaned RHS values.
    // Only remove read-only / side-effect-free instructions; keep anything
    // that could have side effects (calls, stores, etc.).
    if !rhs_ids_to_remove.is_empty() {
        // Iteratively remove orphaned instructions — removing one may orphan
        // another (e.g., `LoadGlobal props` → `PropertyLoad props.default`).
        loop {
            let mut newly_orphaned: HashSet<IdentifierId> = HashSet::new();
            for (_, block) in &mut func.body.blocks {
                block.instructions.retain(|instr| {
                    if !rhs_ids_to_remove.contains(&instr.lvalue.identifier.id) {
                        return true; // keep — not in remove set
                    }
                    let is_side_effect_free = matches!(
                        &instr.value,
                        InstructionValue::Primitive { .. }
                            | InstructionValue::LoadLocal { .. }
                            | InstructionValue::LoadGlobal { .. }
                            | InstructionValue::PropertyLoad { .. }
                            | InstructionValue::ArrayExpression { .. }
                            | InstructionValue::ObjectExpression { .. }
                            | InstructionValue::BinaryExpression { .. }
                            | InstructionValue::UnaryExpression { .. }
                            | InstructionValue::TemplateLiteral { .. }
                            | InstructionValue::TypeCastExpression { .. }
                            | InstructionValue::ComputedLoad { .. }
                    );
                    if is_side_effect_free {
                        // Collect operands of removed instructions — they may
                        // also become orphaned.
                        super::visitors::for_each_instruction_operand(instr, |place| {
                            newly_orphaned.insert(place.identifier.id);
                        });
                        false // remove
                    } else {
                        true // keep — has side effects
                    }
                });
            }
            if newly_orphaned.is_empty() {
                break;
            }
            // Check which newly orphaned IDs are actually unused now.
            let mut still_used: HashSet<IdentifierId> = HashSet::new();
            for (_, block) in &func.body.blocks {
                collect_terminal_used_ids(&block.terminal, &mut still_used);
                for instr in &block.instructions {
                    super::visitors::for_each_instruction_operand(instr, |place| {
                        still_used.insert(place.identifier.id);
                    });
                }
                for phi in &block.phis {
                    for operand in phi.operands.values() {
                        still_used.insert(operand.identifier.id);
                    }
                }
            }
            let mut found_new = false;
            for id in newly_orphaned {
                if !still_used.contains(&id) && rhs_ids_to_remove.insert(id) {
                    found_new = true;
                }
            }
            if !found_new {
                break;
            }
        }
    }
}

/// Collect identifier IDs used as terminal operands.
fn collect_terminal_used_ids(
    terminal: &Terminal,
    used_ids: &mut std::collections::HashSet<IdentifierId>,
) {
    match terminal {
        Terminal::Return { value, .. } => {
            used_ids.insert(value.identifier.id);
        }
        Terminal::Throw { value, .. } => {
            used_ids.insert(value.identifier.id);
        }
        Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
            used_ids.insert(test.identifier.id);
        }
        Terminal::Switch { test, cases, .. } => {
            used_ids.insert(test.identifier.id);
            for case in cases {
                if let Some(t) = &case.test {
                    used_ids.insert(t.identifier.id);
                }
            }
        }
        Terminal::Try {
            handler_binding, ..
        } => {
            if let Some(binding) = handler_binding {
                used_ids.insert(binding.identifier.id);
            }
        }
        _ => {}
    }
}

/// Main entry point: prune MaybeThrow terminals and fix up phi operands.
///
/// After pruning, the pass:
/// 1. Removes unnecessary try/catch terminals (where handler is unreachable)
/// 2. Removes unreachable blocks
/// 3. Recomputes predecessors
/// 4. Re-numbers instruction IDs
/// 5. Rewrites phi operands to reference updated predecessor blocks
pub fn prune_maybe_throws(func: &mut HIRFunction) {
    let terminal_mapping = prune_maybe_throws_impl(func);

    // Always check for unnecessary try/catch, even if no MaybeThrow terminals
    // were pruned. Our HIR lowering doesn't emit MaybeThrow terminals for
    // every instruction inside try bodies (unlike upstream), so Try terminals
    // whose handlers are never referenced by any MaybeThrow should be simplified.
    let try_catch_removed = remove_unnecessary_try_catch(&mut func.body);

    // After removing try-catch blocks, some StoreLocal instructions that were
    // kept alive only because the catch/fallthrough blocks used the variable
    // may now be dead stores. Rewrite them to DeclareLocal (keeping the
    // declaration but dropping the initializer). This matches upstream behavior
    // where DCE's rewriteInstruction runs while try-catch is still present
    // (converting StoreLocal→DeclareLocal), and the DeclareLocal persists
    // because no subsequent DCE pass runs after try-catch removal.
    if try_catch_removed {
        rewrite_dead_stores_after_try_catch_removal(func);
    }

    let terminal_mapping = match terminal_mapping {
        Some(m) => m,
        None => {
            // Even without MaybeThrow pruning, if we removed try/catch, recompute
            mark_predecessors(&mut func.body);
            mark_instruction_ids(&mut func.body);
            return;
        }
    };

    // Recompute predecessors after terminal changes
    mark_predecessors(&mut func.body);

    // Re-number instruction IDs
    mark_instruction_ids(&mut func.body);

    // Rewrite phi operands to reference the updated predecessor blocks.
    // Collect the updates first to avoid borrow issues.
    let phi_updates: Vec<(BlockId, Vec<(BlockId, BlockId)>)> = func
        .body
        .blocks
        .iter()
        .map(|(_, block)| {
            let block_id = block.id;
            let updates: Vec<(BlockId, BlockId)> = block
                .phis
                .iter()
                .flat_map(|phi| {
                    phi.operands.keys().filter_map(|&predecessor| {
                        if !block.preds.contains(&predecessor) {
                            let mapped = terminal_mapping.get(&predecessor).copied();
                            mapped.map(|m| (predecessor, m))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            (block_id, updates)
        })
        .collect();

    for (block_id, updates) in phi_updates {
        if updates.is_empty() {
            continue;
        }
        let block = func
            .body
            .blocks
            .iter_mut()
            .find(|(_, b)| b.id == block_id)
            .map(|(_, b)| b)
            .expect("block must exist");
        for phi in &mut block.phis {
            for &(old_pred, new_pred) in &updates {
                if let Some(operand) = phi.operands.remove(&old_pred) {
                    phi.operands.insert(new_pred, operand);
                }
            }
        }
    }
}
