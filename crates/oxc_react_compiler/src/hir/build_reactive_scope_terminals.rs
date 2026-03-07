//! Build reactive scope terminals in the HIR.
//!
//! Port of `BuildReactiveScopeTerminalsHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Given a function whose reactive scope ranges have been correctly aligned and merged,
//! this pass rewrites blocks to introduce Scope/PrunedScope terminals and their
//! fallthrough blocks. The scope terminal wraps a contiguous range of instructions
//! into a sub-CFG that can later be converted into a reactive scope block.

use std::collections::{HashMap, HashSet};

use super::builder::{each_terminal_successor, reverse_postorder_blocks};
use super::types::*;
use super::visitors::{
    for_each_instruction_lvalue, for_each_instruction_operand, for_each_terminal_operand,
    map_instruction_lvalues, map_instruction_operands, map_terminal_operands,
};

/// Collect all unique reactive scopes referenced by identifiers in the function.
fn get_scopes(func: &HIRFunction) -> Vec<ReactiveScope> {
    let mut seen = HashSet::new();
    let mut scopes = Vec::new();

    let mut visit_place = |place: &Place| {
        if let Some(scope) = &place.identifier.scope
            && scope.range.start != scope.range.end
            && seen.insert(scope.id)
        {
            scopes.push((**scope).clone());
        }
    };

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            for_each_instruction_lvalue(instr, &mut visit_place);
            for_each_instruction_operand(instr, &mut visit_place);
        }
        for_each_terminal_operand(&block.terminal, &mut visit_place);
    }

    scopes
}

/// Sort ranges in ascending order of start instruction, breaking ties
/// with descending order of end instructions (outer ranges before inner).
fn range_pre_order_sort(scopes: &mut [ReactiveScope]) {
    scopes.sort_by(|a, b| {
        let start_diff = a.range.start.0.cmp(&b.range.start.0);
        if start_diff != std::cmp::Ordering::Equal {
            return start_diff;
        }
        // Descending end — outer (larger) ranges come first
        b.range.end.0.cmp(&a.range.end.0)
    });
}

/// Rewrite info for a scope start or end terminal.
#[derive(Debug)]
enum TerminalRewrite {
    StartScope {
        block_id: BlockId,
        fallthrough_id: BlockId,
        instr_id: InstructionId,
        scope: Box<ReactiveScope>,
    },
    EndScope {
        instr_id: InstructionId,
        fallthrough_id: BlockId,
    },
}

impl TerminalRewrite {
    fn instr_id(&self) -> InstructionId {
        match self {
            Self::StartScope { instr_id, .. } | Self::EndScope { instr_id, .. } => *instr_id,
        }
    }
}

/// Generate terminal rewrites by traversing scopes in pre-order.
///
/// This implements the `recursivelyTraverseItems` pattern from upstream:
/// scopes are sorted so nested ones come after their parents, and we
/// emit StartScope/EndScope at the correct instruction IDs.
fn generate_rewrites(
    scopes: &mut [ReactiveScope],
    next_block_id: &mut u32,
) -> Vec<TerminalRewrite> {
    range_pre_order_sort(scopes);

    let mut rewrites = Vec::new();
    let mut fallthrough_map: HashMap<ScopeId, BlockId> = HashMap::new();
    let mut active: Vec<usize> = Vec::new(); // indices into scopes

    for i in 0..scopes.len() {
        let curr_start = scopes[i].range.start;
        let curr_end = scopes[i].range.end;
        let mut skip_scope = false;

        // Close scopes that are disjoint with current
        while let Some(&parent_idx) = active.last() {
            let parent_end = scopes[parent_idx].range.end;
            if curr_start.0 >= parent_end.0 {
                // Disjoint — emit end
                let ft = fallthrough_map[&scopes[parent_idx].id];
                rewrites.push(TerminalRewrite::EndScope {
                    instr_id: parent_end,
                    fallthrough_id: ft,
                });
                active.pop();
            } else {
                // Nested — current is inside parent
                if curr_end.0 > scopes[parent_idx].range.end.0 {
                    // Overlapping scopes that weren't merged — skip this scope.
                    skip_scope = true;
                }
                break;
            }
        }

        if skip_scope {
            continue;
        }

        // Emit start for current scope
        let block_id = BlockId::new(*next_block_id);
        *next_block_id += 1;
        let fallthrough_id = BlockId::new(*next_block_id);
        *next_block_id += 1;

        fallthrough_map.insert(scopes[i].id, fallthrough_id);
        rewrites.push(TerminalRewrite::StartScope {
            block_id,
            fallthrough_id,
            instr_id: curr_start,
            scope: Box::new(scopes[i].clone()),
        });
        active.push(i);
    }

    // Close remaining active scopes
    while let Some(parent_idx) = active.pop() {
        let ft = fallthrough_map[&scopes[parent_idx].id];
        rewrites.push(TerminalRewrite::EndScope {
            instr_id: scopes[parent_idx].range.end,
            fallthrough_id: ft,
        });
    }

    // Sort by instruction ID (ascending)
    rewrites.sort_by_key(|r| r.instr_id().0);
    rewrites
}

/// Per-block rewrite state.
struct BlockRewriteState {
    source_kind: BlockKind,
    next_block_id: BlockId,
    next_preds: HashSet<BlockId>,
    instr_slice_idx: usize,
    // Exact source instruction slice assigned to each rewrite block.
    // Mirrors upstream `handleRewrite(..., idx, context)` slicing semantics.
    slice_ranges: Vec<(usize, usize)>,
    new_blocks: Vec<(BlockId, BasicBlock)>,
}

/// Build reactive scope terminals in the HIR.
///
/// This is the main entry point. It:
/// 1. Collects all reactive scopes from identifiers
/// 2. Generates StartScope/EndScope rewrites at the appropriate instruction IDs
/// 3. Splits blocks to insert Scope terminals and Goto terminals
/// 4. Fixes up phi nodes, predecessors, and instruction IDs
pub fn build_reactive_scope_terminals(func: &mut HIRFunction) {
    // Step 1: Collect and sort scopes
    let mut scopes = get_scopes(func);
    if scopes.is_empty() {
        return;
    }

    if std::env::var("DEBUG_BUILD_SCOPE_TERMINALS").is_ok() {
        eprintln!(
            "[BUILD_SCOPE_TERMINALS] input_scopes={}",
            scopes
                .iter()
                .map(|s| format!("{}:({},{})", s.id.0, s.range.start.0, s.range.end.0))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Find the maximum block ID to generate new unique IDs
    let mut max_block_id = 0u32;
    for (bid, _) in &func.body.blocks {
        if bid.0 > max_block_id {
            max_block_id = bid.0;
        }
    }
    let mut next_block_id = max_block_id + 1;

    // Step 2: Generate rewrites
    let mut rewrites = generate_rewrites(&mut scopes, &mut next_block_id);
    if rewrites.is_empty() {
        return;
    }

    if std::env::var("DEBUG_BUILD_SCOPE_TERMINALS").is_ok() {
        let rewrite_dump = rewrites
            .iter()
            .map(|r| match r {
                TerminalRewrite::StartScope {
                    block_id,
                    fallthrough_id,
                    instr_id,
                    scope,
                } => format!(
                    "start@{} scope={} block={} ft={}",
                    instr_id.0, scope.id.0, block_id.0, fallthrough_id.0
                ),
                TerminalRewrite::EndScope {
                    instr_id,
                    fallthrough_id,
                } => format!("end@{} ft={}", instr_id.0, fallthrough_id.0),
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("[BUILD_SCOPE_TERMINALS] rewrites={rewrite_dump}");
    }

    // Reverse so we can pop from the end as we traverse in ascending order
    rewrites.reverse();

    // Step 3: Apply rewrites by splitting blocks
    let mut next_blocks: Vec<(BlockId, BasicBlock)> = Vec::new();
    let mut rewritten_final_blocks: HashMap<BlockId, BlockId> = HashMap::new();

    // Take ownership of blocks
    let old_blocks = std::mem::take(&mut func.body.blocks);

    for (block_id, mut block) in old_blocks {
        let mut ctx = BlockRewriteState {
            source_kind: block.kind,
            next_block_id: block_id,
            next_preds: block.preds.clone(),
            instr_slice_idx: 0,
            slice_ranges: Vec::new(),
            new_blocks: Vec::new(),
        };

        // Process each instruction position looking for rewrites
        let num_instrs = block.instructions.len();
        for i in 0..=num_instrs {
            let instr_id = if i < num_instrs {
                block.instructions[i].id
            } else {
                block.terminal.id()
            };

            while let Some(next_rewrite) = rewrites.last() {
                if next_rewrite.instr_id().0 <= instr_id.0 {
                    let rewrite = rewrites.pop().unwrap();
                    let terminal = match &rewrite {
                        TerminalRewrite::StartScope {
                            fallthrough_id,
                            block_id,
                            scope,
                            instr_id,
                            ..
                        } => Terminal::Scope {
                            fallthrough: *fallthrough_id,
                            block: *block_id,
                            scope: *scope.clone(),
                            id: *instr_id,
                            loc: SourceLocation::Generated,
                        },
                        TerminalRewrite::EndScope {
                            fallthrough_id,
                            instr_id,
                            ..
                        } => Terminal::Goto {
                            block: *fallthrough_id,
                            variant: GotoVariant::Break,
                            id: *instr_id,
                            loc: SourceLocation::Generated,
                        },
                    };

                    let curr_block_id = ctx.next_block_id;
                    let slice_start = ctx.instr_slice_idx;
                    let slice_end = i;

                    // Only first block gets the original phis
                    let phis = if ctx.new_blocks.is_empty() {
                        std::mem::take(&mut block.phis)
                    } else {
                        Vec::new()
                    };

                    // We can't easily slice out instructions since we don't own them yet
                    // We'll collect indices and do the actual splitting after
                    ctx.new_blocks.push((
                        curr_block_id,
                        BasicBlock {
                            kind: ctx.source_kind,
                            id: curr_block_id,
                            instructions: Vec::new(), // will be filled below
                            terminal,
                            preds: ctx.next_preds.clone(),
                            phis,
                        },
                    ));
                    ctx.slice_ranges.push((slice_start, slice_end));

                    // Record the instruction range for this block
                    // (stored as a tag in the block's instructions temporarily)

                    ctx.next_preds = {
                        let mut s = HashSet::new();
                        s.insert(curr_block_id);
                        s
                    };

                    ctx.next_block_id = match &rewrite {
                        TerminalRewrite::StartScope { block_id, .. } => *block_id,
                        TerminalRewrite::EndScope { fallthrough_id, .. } => *fallthrough_id,
                    };

                    ctx.instr_slice_idx = i;
                } else {
                    break;
                }
            }
        }

        if !ctx.new_blocks.is_empty() {
            // Distribute source instruction slices exactly as upstream does:
            // each rewrite block gets `source.instructions[instrSliceIdx..idx]`.
            let all_instructions = std::mem::take(&mut block.instructions);
            for ((_, new_block), (start, end)) in
                ctx.new_blocks.iter_mut().zip(ctx.slice_ranges.iter())
            {
                new_block.instructions = all_instructions[*start..*end].to_vec();
            }

            // Final block gets the trailing slice plus original terminal.
            let remaining: Vec<Instruction> = all_instructions[ctx.instr_slice_idx..].to_vec();
            let final_block_phis = if ctx.new_blocks.is_empty() {
                std::mem::take(&mut block.phis)
            } else {
                Vec::new()
            };

            let final_block = BasicBlock {
                kind: ctx.source_kind,
                id: ctx.next_block_id,
                instructions: remaining,
                terminal: block.terminal,
                preds: ctx.next_preds,
                phis: final_block_phis,
            };

            for (bid, b) in ctx.new_blocks {
                next_blocks.push((bid, b));
            }
            rewritten_final_blocks.insert(block_id, final_block.id);
            next_blocks.push((final_block.id, final_block));
        } else {
            next_blocks.push((block_id, block));
        }
    }

    func.body.blocks = next_blocks;

    // Step 4: Repoint phi operands that refer to rewritten blocks
    for (_, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            let updates: Vec<(BlockId, BlockId)> = phi
                .operands
                .keys()
                .filter_map(|original_id| {
                    rewritten_final_blocks
                        .get(original_id)
                        .map(|new_id| (*original_id, *new_id))
                })
                .collect();

            for (old_id, new_id) in updates {
                if let Some(value) = phi.operands.remove(&old_id) {
                    phi.operands.insert(new_id, value);
                }
            }
        }
    }

    // Step 5: Restore reverse-postorder block layout (upstream Step 4)
    reverse_postorder_blocks(&mut func.body);

    // Step 6: Recompute predecessors
    recompute_predecessors(&mut func.body);

    // Step 7: Renumber instruction IDs
    renumber_instructions(&mut func.body);

    // Step 8: Fix scope and identifier ranges
    fix_scope_and_identifier_ranges(&mut func.body);
}

/// Recompute predecessor sets for all blocks.
fn recompute_predecessors(body: &mut HIR) {
    let block_ids: Vec<BlockId> = body.blocks.iter().map(|(id, _)| *id).collect();
    let mut pred_map: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    for bid in &block_ids {
        pred_map.insert(*bid, HashSet::new());
    }
    for (_, block) in &body.blocks {
        for succ in each_terminal_successor(&block.terminal) {
            if let Some(preds) = pred_map.get_mut(&succ) {
                preds.insert(block.id);
            }
        }
    }
    for (_, block) in &mut body.blocks {
        if let Some(preds) = pred_map.remove(&block.id) {
            block.preds = preds;
        }
    }
}

/// Renumber all instruction IDs sequentially.
fn renumber_instructions(body: &mut HIR) {
    let mut instr_id = 0u32;
    for (_, block) in &mut body.blocks {
        for instr in &mut block.instructions {
            instr_id += 1;
            instr.id = InstructionId::new(instr_id);
        }
        instr_id += 1;
        assign_terminal_id_inline(&mut block.terminal, InstructionId::new(instr_id));
    }
}

fn assign_terminal_id_inline(terminal: &mut Terminal, id: InstructionId) {
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

/// Fix scope and identifier ranges to account for renumbered instructions.
/// After renumbering, we need to update the mutable_range on identifiers
/// and the range on reactive scopes to match the new instruction IDs.
pub(crate) fn fix_scope_and_identifier_ranges(body: &mut HIR) {
    // Upstream `fixScopeAndIdentifierRanges` updates each scope terminal so that:
    // - scope.range.start == scope terminal id
    // - scope.range.end == first instruction id of the fallthrough block
    //
    // In upstream TS, places hold shared references to the same scope objects,
    // so this also updates all identifier-attached scopes transitively.
    // Rust stores cloned scopes per identifier, so we explicitly propagate by ScopeId.

    // Step 1: Compute canonical range per scope from scope/pruned-scope terminals.
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

    // Step 2: Apply canonical ranges to terminals and all identifier scope clones.
    for (_, block) in &mut body.blocks {
        for phi in &mut block.phis {
            update_identifier_scope_range(&mut phi.place.identifier, &scope_ranges);
            for operand in phi.operands.values_mut() {
                update_identifier_scope_range(&mut operand.identifier, &scope_ranges);
            }
        }

        for instr in &mut block.instructions {
            map_instruction_lvalues(instr, |place| {
                update_identifier_scope_range(&mut place.identifier, &scope_ranges);
            });
            map_instruction_operands(instr, |place| {
                update_identifier_scope_range(&mut place.identifier, &scope_ranges);
            });
        }

        match &mut block.terminal {
            Terminal::Scope { scope, .. } | Terminal::PrunedScope { scope, .. } => {
                if let Some(range) = scope_ranges.get(&scope.id) {
                    scope.range = range.clone();
                }
            }
            _ => {}
        }

        map_terminal_operands(&mut block.terminal, |place| {
            update_identifier_scope_range(&mut place.identifier, &scope_ranges);
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
        // Upstream assigns `identifier.mutableRange = scope.range` by reference.
        // Keep Rust semantics equivalent by explicit copy.
        identifier.mutable_range = range.clone();
    }
}
