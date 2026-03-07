//! Constant Propagation
//!
//! Port of `ConstantPropagation.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Applies constant propagation/folding: tracks known constant values for identifiers,
//! replaces instructions whose operands are known constants with the computed result.
//! Also prunes unreachable branches when an if-condition is a known constant.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::hir::types::*;

/// A known constant value for an identifier.
#[derive(Debug, Clone)]
enum Constant {
    Primitive(PrimitiveValue, SourceLocation),
    LoadGlobal(NonLocalBinding, SourceLocation),
}

type Constants = HashMap<IdentifierId, Constant>;

struct EvaluationContext<'a> {
    update_value_ids: &'a HashSet<IdentifierId>,
    for_loop_var_decl_ids: &'a HashSet<DeclarationId>,
    multi_reassign_decl_ids: &'a HashSet<DeclarationId>,
    context_reassign_decl_ids: &'a HashSet<DeclarationId>,
    mutation_receiver_ids: &'a HashSet<IdentifierId>,
    load_local_source: &'a HashMap<IdentifierId, IdentifierId>,
    mutated_captured_decl_ids: &'a HashSet<DeclarationId>,
    captured_reassigned_decl_ids: &'a HashSet<DeclarationId>,
    reassigned_decl_ids: &'a HashSet<DeclarationId>,
}

#[inline]
fn debug_cp_trace_enabled() -> bool {
    std::env::var("DEBUG_CP_TRACE").is_ok()
}

fn capture_may_mutate(effect: Effect) -> bool {
    matches!(
        effect,
        Effect::Store
            | Effect::Mutate
            | Effect::ConditionallyMutate
            | Effect::ConditionallyMutateIterator
    )
}

fn collect_reassigned_decl_ids_in_function(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    out.insert(lvalue.identifier.declaration_id);
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_reassigned_decl_ids_in_function(&lowered_func.func, out);
                }
                _ => {}
            }
        }
    }
}

/// Run constant propagation on the given function (fixpoint iteration).
/// After branch pruning, unreachable phi operands are removed and the pass
/// is re-run so that phis with fewer operands can resolve to constants.
pub fn constant_propagation(func: &mut HIRFunction) {
    // Iterate until no more branches are pruned
    for _ in 0..10 {
        let mut constants: Constants = HashMap::new();
        let pruned = apply_constant_propagation(func, &mut constants);
        if pruned == 0 {
            break;
        }
        // After pruning, update phi operands and eliminate dead blocks so that
        // the next iteration's multi_reassign_decl_ids computation only sees
        // reachable stores (stores in pruned dead branches should not count).
        update_phi_operands_after_pruning(func);
        eliminate_dead_blocks(func);
    }
}

/// Remove blocks unreachable from the entry block.
fn eliminate_dead_blocks(func: &mut HIRFunction) {
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
            for succ in crate::hir::builder::terminal_successors(terminal) {
                if reachable.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }
    let before = func.body.blocks.len();
    func.body.blocks.retain(|(id, _)| reachable.contains(id));
    if func.body.blocks.len() < before {
        // Update predecessor sets after removing blocks
        update_phi_operands_after_pruning(func);
    }
}

/// Remove phi operands that reference blocks which are no longer predecessors.
fn update_phi_operands_after_pruning(func: &mut HIRFunction) {
    use std::collections::HashSet;

    // Compute reachable blocks from entry first — unreachable blocks
    // (e.g., dead if-branches after pruning) must not count as predecessors.
    let reachable: HashSet<BlockId> = {
        let block_id_to_index: HashMap<BlockId, usize> = func
            .body
            .blocks
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (*id, i))
            .collect();
        let mut visited: HashSet<BlockId> = HashSet::new();
        let mut queue: VecDeque<BlockId> = VecDeque::new();
        queue.push_back(func.body.entry);
        visited.insert(func.body.entry);
        while let Some(bid) = queue.pop_front() {
            if let Some(&idx) = block_id_to_index.get(&bid) {
                let terminal = &func.body.blocks[idx].1.terminal;
                for succ in crate::hir::builder::terminal_successors(terminal) {
                    if visited.insert(succ) {
                        queue.push_back(succ);
                    }
                }
            }
        }
        visited
    };

    // Rebuild predecessor sets from terminals of REACHABLE blocks only
    let mut actual_preds: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    for (block_id, block) in &func.body.blocks {
        if !reachable.contains(block_id) {
            continue;
        }
        let succs = crate::hir::builder::terminal_successors(&block.terminal);
        for succ in succs {
            actual_preds.entry(succ).or_default().insert(*block_id);
        }
    }

    // For each block with phis, remove operands from non-predecessor blocks
    for (block_id, block) in &mut func.body.blocks {
        let preds = actual_preds.get(block_id).cloned().unwrap_or_default();
        for phi in &mut block.phis {
            phi.operands.retain(|pred_id, _| preds.contains(pred_id));
        }
        // Also update block.preds
        block.preds = preds.into_iter().collect();
    }
}

fn collect_named_declarations(func: &HIRFunction) -> HashMap<String, DeclarationId> {
    let mut map = HashMap::new();
    for param in &func.params {
        let ident = match param {
            Argument::Place(place) => &place.identifier,
            Argument::Spread(place) => &place.identifier,
        };
        if let Some(name) = &ident.name {
            map.insert(name.value().to_string(), ident.declaration_id);
        }
    }
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            crate::hir::visitors::for_each_instruction_lvalue(instr, |place| {
                if let Some(name) = &place.identifier.name {
                    map.insert(name.value().to_string(), place.identifier.declaration_id);
                }
            });
        }
    }
    map
}

fn collect_mutated_captured_decl_ids(func: &HIRFunction) -> HashSet<DeclarationId> {
    let name_to_decl = collect_named_declarations(func);
    let mut mutated = HashSet::new();
    collect_mutated_captured_decl_ids_with_map(func, &name_to_decl, &mut mutated);
    mutated
}

fn collect_mutated_captured_decl_ids_with_map(
    func: &HIRFunction,
    name_to_decl: &HashMap<String, DeclarationId>,
    out: &mut HashSet<DeclarationId>,
) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreGlobal { name, .. } => {
                    if let Some(decl_id) = name_to_decl.get(name) {
                        out.insert(*decl_id);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_mutated_captured_decl_ids_with_map(
                        &lowered_func.func,
                        name_to_decl,
                        out,
                    );
                }
                _ => {}
            }
        }
    }
}

fn collect_syntactic_captured_decl_ids_with_map(
    func: &HIRFunction,
    name_to_decl: &HashMap<String, DeclarationId>,
    out: &mut HashSet<DeclarationId>,
) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name },
                    ..
                } => {
                    if let Some(decl_id) = name_to_decl.get(name) {
                        out.insert(*decl_id);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_syntactic_captured_decl_ids_with_map(
                        &lowered_func.func,
                        name_to_decl,
                        out,
                    );
                }
                _ => {}
            }
        }
    }
}

fn collect_captured_decl_ids(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    for captured in &lowered_func.func.context {
                        out.insert(captured.identifier.declaration_id);
                    }
                    collect_captured_decl_ids(&lowered_func.func, out);
                }
                _ => {}
            }
        }
    }
}

/// Returns the number of if-terminals that were pruned to gotos.
fn apply_constant_propagation(func: &mut HIRFunction, constants: &mut Constants) -> usize {
    let mut pruned = 0;
    let mutated_captured_decl_ids = collect_mutated_captured_decl_ids(func);

    // Collect identifier IDs that are used as `value` operands of PrefixUpdate/PostfixUpdate.
    // These LoadLocal instructions must NOT be rewritten to Primitive, because codegen
    // needs the variable reference (e.g., `i++` not `0++`).
    let mut update_value_ids: HashSet<IdentifierId> = HashSet::new();
    // Collect identifier IDs whose computed value is consumed by mutation-style
    // operations. Rewriting those loads to aliased sources changes emitted alias
    // shape in nested closures (e.g. `a_0.x = b` -> `y.x = x`).
    let mut mutation_receiver_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::PrefixUpdate { value, .. }
                | InstructionValue::PostfixUpdate { value, .. } => {
                    update_value_ids.insert(value.identifier.id);
                }
                InstructionValue::PropertyStore { object, value, .. } => {
                    mutation_receiver_ids.insert(object.identifier.id);
                    mutation_receiver_ids.insert(value.identifier.id);
                }
                InstructionValue::PropertyDelete { object, .. } => {
                    mutation_receiver_ids.insert(object.identifier.id);
                }
                InstructionValue::ComputedStore { object, value, .. } => {
                    mutation_receiver_ids.insert(object.identifier.id);
                    mutation_receiver_ids.insert(value.identifier.id);
                }
                InstructionValue::ComputedDelete { object, .. } => {
                    mutation_receiver_ids.insert(object.identifier.id);
                }
                InstructionValue::MethodCall {
                    receiver, args: _, ..
                } => {
                    mutation_receiver_ids.insert(receiver.identifier.id);
                }
                _ => {}
            }
        }
    }

    // Map from LoadLocal temp lvalue ID → named variable's identifier ID.
    // Used so PrefixUpdate/PostfixUpdate can propagate the new constant back
    // to the named variable that subsequent LoadLocals will read from.
    let mut load_local_source: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    // Map IdentifierId -> DeclarationId for both lvalues and referenced places.
    // Needed so declaration-scoped invalidation (e.g. context captures) can clear
    // all constants tied to the same logical variable across SSA ids.
    let mut decl_for_id: HashMap<IdentifierId, DeclarationId> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            decl_for_id.insert(
                instr.lvalue.identifier.id,
                instr.lvalue.identifier.declaration_id,
            );
            match &instr.value {
                InstructionValue::LoadLocal { place, .. } => {
                    load_local_source.insert(instr.lvalue.identifier.id, place.identifier.id);
                    decl_for_id.insert(place.identifier.id, place.identifier.declaration_id);
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    decl_for_id.insert(
                        lvalue.place.identifier.id,
                        lvalue.place.identifier.declaration_id,
                    );
                }
                _ => {}
            }
        }
    }

    // Collect declaration IDs of variables that are reassigned multiple times
    // WITHIN THE SAME BLOCK that also contains Ternary/LogicalExpression
    // instructions. Our HIR lowering evaluates both sides of logical/ternary
    // expressions eagerly (unlike upstream which uses control flow), so a variable
    // assigned in both branches (e.g., `(x = 1) && (x = 2)`) would incorrectly
    // be folded to the last assignment. We prevent LoadLocal replacement for these
    // variables. Sequential reassignments (e.g., `x = 1; x = x+1; x = x+1`)
    // without ternary/logical are correctly handled by SSA and should NOT be blocked.
    let multi_reassign_decl_ids: HashSet<DeclarationId> = {
        let mut result: HashSet<DeclarationId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            // Check if this block contains any Ternary/LogicalExpression instructions
            let has_ternary_or_logical = block.instructions.iter().any(|instr| {
                matches!(
                    &instr.value,
                    InstructionValue::Ternary { .. } | InstructionValue::LogicalExpression { .. }
                )
            });
            if !has_ternary_or_logical {
                continue;
            }
            let mut block_reassign_counts: HashMap<DeclarationId, usize> = HashMap::new();
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && lvalue.kind == InstructionKind::Reassign
                    && lvalue.place.identifier.name.is_some()
                {
                    *block_reassign_counts
                        .entry(lvalue.place.identifier.declaration_id)
                        .or_insert(0) += 1;
                }
            }
            for (decl_id, count) in block_reassign_counts {
                if count > 1 {
                    result.insert(decl_id);
                }
            }
        }
        result
    };

    // Declarations that are reassigned through StoreContext can keep older
    // declaration-rooted LoadLocal ids alive across SSA versions in our port.
    // Avoid folding those LoadLocals to stale constants.
    let mut context_reassign_decl_ids: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StoreContext { lvalue, .. } = &instr.value
                && lvalue.kind == InstructionKind::Reassign
                && lvalue.place.identifier.name.is_some()
            {
                context_reassign_decl_ids.insert(lvalue.place.identifier.declaration_id);
            }
        }
    }

    // Captured reads in nested functions must not observe stale parent constants
    // when the captured declaration is reassigned anywhere in the parent function.
    // Our lowering can keep captured loads keyed to declaration-rooted IDs, so
    // without this invalidation a nested closure can incorrectly fold to an
    // earlier initializer value (e.g. `let x = null; ...; x = {};`).
    let mut reassigned_decl_ids: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        reassigned_decl_ids.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                _ => {}
            }
        }
    }
    let name_to_decl = collect_named_declarations(func);
    let mut syntactic_captured_decl_ids: HashSet<DeclarationId> = HashSet::new();
    collect_syntactic_captured_decl_ids_with_map(
        func,
        &name_to_decl,
        &mut syntactic_captured_decl_ids,
    );
    let mut captured_decl_ids: HashSet<DeclarationId> = HashSet::new();
    collect_captured_decl_ids(func, &mut captured_decl_ids);
    captured_decl_ids.extend(syntactic_captured_decl_ids.iter().copied());
    let captured_reassigned_decl_ids: HashSet<DeclarationId> = captured_decl_ids
        .intersection(&reassigned_decl_ids)
        .copied()
        .collect();
    if std::env::var("DEBUG_CP_CONTEXT").is_ok() && !captured_decl_ids.is_empty() {
        let mut captured = captured_decl_ids.iter().map(|id| id.0).collect::<Vec<_>>();
        captured.sort_unstable();
        let mut reassigned = reassigned_decl_ids
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>();
        reassigned.sort_unstable();
        let mut captured_reassigned = captured_reassigned_decl_ids
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>();
        captured_reassigned.sort_unstable();
        eprintln!(
            "[CP_CONTEXT] captured_decls={:?} reassigned_decls={:?} captured_reassigned={:?}",
            captured, reassigned, captured_reassigned
        );
    }

    // Collect declaration IDs of loop variables that are actually MODIFIED
    // in loop bodies/update clauses. These should NOT be propagated by CP
    // because back-edges create values our single-pass BFS doesn't model.
    let mut for_loop_var_decl_ids: HashSet<DeclarationId> = HashSet::new();
    {
        // Build id_to_decl for LoadLocal tracing
        let mut id_to_decl_cp: HashMap<IdentifierId, DeclarationId> = HashMap::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::LoadLocal { place, .. } = &instr.value
                    && place.identifier.name.is_some()
                {
                    id_to_decl_cp
                        .insert(instr.lvalue.identifier.id, place.identifier.declaration_id);
                }
            }
        }

        // Collect for-loop init decl IDs and loop body/update block IDs
        let mut for_init_decl_ids: HashSet<DeclarationId> = HashSet::new();
        let mut loop_body_blocks: HashSet<BlockId> = HashSet::new();
        let mut loop_test_blocks: HashSet<BlockId> = HashSet::new();
        let mut loop_continue_target_blocks: HashSet<BlockId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    let continue_target = update.unwrap_or(*test);
                    loop_continue_target_blocks.insert(continue_target);
                    if let Some(u) = update {
                        loop_body_blocks.insert(*u);
                    }
                    loop_body_blocks.insert(*loop_block);
                    loop_test_blocks.insert(*test);
                    // Collect init variable declaration IDs
                    if let Some((_, init_block)) =
                        func.body.blocks.iter().find(|(id, _)| id == init)
                    {
                        for instr in &init_block.instructions {
                            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                                && lvalue.place.identifier.name.is_some()
                            {
                                for_init_decl_ids.insert(lvalue.place.identifier.declaration_id);
                            }
                        }
                    }
                }
                Terminal::While {
                    test, loop_block, ..
                }
                | Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    loop_body_blocks.insert(*loop_block);
                    loop_test_blocks.insert(*test);
                    loop_continue_target_blocks.insert(*test);
                }
                Terminal::ForOf {
                    loop_block, init, ..
                } => {
                    loop_body_blocks.insert(*loop_block);
                    loop_body_blocks.insert(*init);
                    loop_continue_target_blocks.insert(*init);
                }
                Terminal::ForIn {
                    loop_block, init, ..
                } => {
                    loop_body_blocks.insert(*loop_block);
                    loop_body_blocks.insert(*init);
                    loop_continue_target_blocks.insert(*init);
                }
                _ => {}
            }
        }

        // Include feeder blocks that reach known loop body/test/continue blocks.
        // Lowered loop bodies can contain nested `if`/`switch` blocks whose
        // internal gotos are not marked as `Continue`, so walk this to a fixed
        // point instead of only following direct continue edges.
        let mut changed = true;
        while changed {
            changed = false;
            for (bid, block) in &func.body.blocks {
                if let Terminal::Goto { block: target, .. } = &block.terminal
                    && (loop_test_blocks.contains(target)
                        || loop_continue_target_blocks.contains(target)
                        || loop_body_blocks.contains(target))
                {
                    changed |= loop_body_blocks.insert(*bid);
                }
            }
        }

        // Find all variables modified inside loop body/update blocks
        for (_, block) in &func.body.blocks {
            if !loop_body_blocks.contains(&block.id) {
                continue;
            }
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        // Only protect variables that are REASSIGNED in loop bodies
                        // (not initial declarations, which are fresh each iteration)
                        if lvalue.kind == InstructionKind::Reassign
                            && lvalue.place.identifier.name.is_some()
                        {
                            for_loop_var_decl_ids.insert(lvalue.place.identifier.declaration_id);
                        }
                    }
                    InstructionValue::PrefixUpdate { value, .. }
                    | InstructionValue::PostfixUpdate { value, .. } => {
                        if let Some(&decl_id) = id_to_decl_cp.get(&value.identifier.id) {
                            for_loop_var_decl_ids.insert(decl_id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Match upstream SCCP traversal: walk the CFG in stored block order.
    for (_, block) in &mut func.body.blocks {
        // Initialize phi values if all operands have the same known constant value
        for phi in &block.phis {
            if let Some(value) = evaluate_phi(phi, constants) {
                constants.insert(phi.place.identifier.id, value);
            }
        }

        let len = block.instructions.len();
        for i in 0..len {
            // Match upstream: only skip the terminal value instruction in sequence blocks.
            if block.kind == BlockKind::Sequence && i == len - 1 {
                continue;
            }

            let instr = &mut block.instructions[i];
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    let mut nested_reassigned_decl_ids: HashSet<DeclarationId> = HashSet::new();
                    collect_reassigned_decl_ids_in_function(
                        &lowered_func.func,
                        &mut nested_reassigned_decl_ids,
                    );

                    // Captured context values that can be mutated by the nested
                    // function must not remain constant in the parent function.
                    // Also clear constants for captured declarations that are
                    // reassigned in the parent function: captured reads are
                    // closure reads, not snapshot values at creation time.
                    for captured in &lowered_func.func.context {
                        let captured_decl = captured.identifier.declaration_id;
                        let invalidate_for_effect = capture_may_mutate(captured.effect)
                            || nested_reassigned_decl_ids.contains(&captured_decl);
                        let invalidate_for_parent_reassign =
                            reassigned_decl_ids.contains(&captured_decl);
                        if !invalidate_for_effect && !invalidate_for_parent_reassign {
                            continue;
                        }
                        let debug_context = std::env::var("DEBUG_CP_CONTEXT").is_ok();
                        let before_matching: Vec<String> = if debug_context {
                            let mut rows: Vec<String> = constants
                                .iter()
                                .filter_map(|(id, value)| {
                                    let same_decl = decl_for_id
                                        .get(id)
                                        .is_some_and(|decl| *decl == captured_decl);
                                    let source_same_decl = load_local_source
                                        .get(id)
                                        .and_then(|source| decl_for_id.get(source))
                                        .is_some_and(|decl| *decl == captured_decl);
                                    if same_decl || source_same_decl {
                                        Some(format!("id={} const={:?}", id.0, value))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            rows.sort();
                            rows
                        } else {
                            Vec::new()
                        };
                        constants.retain(|id, _| {
                            let same_decl = decl_for_id
                                .get(id)
                                .is_some_and(|decl| *decl == captured_decl);
                            let source_same_decl = load_local_source
                                .get(id)
                                .and_then(|source| decl_for_id.get(source))
                                .is_some_and(|decl| *decl == captured_decl);
                            !same_decl && !source_same_decl
                        });
                        if debug_context {
                            let mut after_matching: Vec<String> = constants
                                .iter()
                                .filter_map(|(id, value)| {
                                    let same_decl = decl_for_id
                                        .get(id)
                                        .is_some_and(|decl| *decl == captured_decl);
                                    let source_same_decl = load_local_source
                                        .get(id)
                                        .and_then(|source| decl_for_id.get(source))
                                        .is_some_and(|decl| *decl == captured_decl);
                                    if same_decl || source_same_decl {
                                        Some(format!("id={} const={:?}", id.0, value))
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            after_matching.sort();
                            eprintln!(
                                "[CP_CONTEXT] invalidate captured id={} decl={} effect={:?} parent_reassign={} before={} after={} before_entries={:?} after_entries={:?}",
                                captured.identifier.id.0,
                                captured.identifier.declaration_id.0,
                                captured.effect,
                                invalidate_for_parent_reassign,
                                before_matching.len(),
                                after_matching.len(),
                                before_matching,
                                after_matching
                            );
                        }
                    }
                    // Upstream recurses CP into nested lowered functions using
                    // the same constants map.
                    apply_constant_propagation(&mut lowered_func.func, constants);
                    continue;
                }
                _ => {}
            }
            let eval_ctx = EvaluationContext {
                update_value_ids: &update_value_ids,
                for_loop_var_decl_ids: &for_loop_var_decl_ids,
                multi_reassign_decl_ids: &multi_reassign_decl_ids,
                context_reassign_decl_ids: &context_reassign_decl_ids,
                mutation_receiver_ids: &mutation_receiver_ids,
                load_local_source: &load_local_source,
                mutated_captured_decl_ids: &mutated_captured_decl_ids,
                captured_reassigned_decl_ids: &captured_reassigned_decl_ids,
                reassigned_decl_ids: &reassigned_decl_ids,
            };
            if let Some(value) = evaluate_instruction(constants, instr, &eval_ctx) {
                constants.insert(instr.lvalue.identifier.id, value);
            }
        }

        // Prune constant if-terminals
        if let Terminal::If {
            test,
            consequent,
            alternate,
            id,
            loc,
            ..
        } = &block.terminal
            && let Some(Constant::Primitive(pv, _)) = read(constants, test)
        {
            let is_truthy = match &pv {
                PrimitiveValue::Boolean(b) => *b,
                PrimitiveValue::Number(n) => *n != 0.0 && !n.is_nan(),
                PrimitiveValue::String(s) => !s.is_empty(),
                PrimitiveValue::Null | PrimitiveValue::Undefined => false,
            };
            let target = if is_truthy { *consequent } else { *alternate };
            let id = *id;
            let loc = loc.clone();
            block.terminal = Terminal::Goto {
                block: target,
                variant: GotoVariant::Break,
                id,
                loc,
            };
            pruned += 1;
        }
    }
    pruned
}

fn evaluate_phi(phi: &Phi, constants: &Constants) -> Option<Constant> {
    let mut value: Option<Constant> = None;
    for operand in phi.operands.values() {
        let operand_value = constants.get(&operand.identifier.id)?;
        match (&value, operand_value) {
            (None, c) => {
                value = Some(c.clone());
            }
            (Some(Constant::Primitive(pv1, _)), Constant::Primitive(pv2, _)) => {
                if !primitive_eq(pv1, pv2) {
                    return None;
                }
            }
            (Some(Constant::LoadGlobal(b1, _)), Constant::LoadGlobal(b2, _)) => {
                if non_local_binding_name(b1) != non_local_binding_name(b2) {
                    return None;
                }
            }
            _ => return None,
        }
    }
    value
}

fn evaluate_instruction(
    constants: &mut Constants,
    instr: &mut Instruction,
    ctx: &EvaluationContext<'_>,
) -> Option<Constant> {
    match &instr.value {
        InstructionValue::Primitive { value, loc } => {
            Some(Constant::Primitive(value.clone(), loc.clone()))
        }
        InstructionValue::LoadGlobal { binding, loc } => {
            Some(Constant::LoadGlobal(binding.clone(), loc.clone()))
        }
        InstructionValue::LoadLocal { place, .. } => {
            if debug_cp_trace_enabled() {
                eprintln!(
                    "[CP_TRACE] loadlocal guard instr={} place_id={} place_decl={} captured_reassigned_contains={} set_len={}",
                    instr.id.0,
                    place.identifier.id.0,
                    place.identifier.declaration_id.0,
                    ctx.captured_reassigned_decl_ids
                        .contains(&place.identifier.declaration_id),
                    ctx.captured_reassigned_decl_ids.len()
                );
            }
            if ctx
                .captured_reassigned_decl_ids
                .contains(&place.identifier.declaration_id)
            {
                if debug_cp_trace_enabled() {
                    eprintln!(
                        "[CP_TRACE] skip LoadLocal fold due to captured+reassigned instr={} place_id={} place_decl={}",
                        instr.id.0, place.identifier.id.0, place.identifier.declaration_id.0
                    );
                }
                return None;
            }
            if place.identifier.name.is_some()
                && ctx
                    .mutated_captured_decl_ids
                    .contains(&place.identifier.declaration_id)
            {
                return None;
            }
            if place.identifier.name.is_some()
                && ctx
                    .context_reassign_decl_ids
                    .contains(&place.identifier.declaration_id)
            {
                if debug_cp_trace_enabled() {
                    eprintln!(
                        "[CP_TRACE] skip LoadLocal fold due to StoreContext reassignment instr={} place_id={} place_decl={}",
                        instr.id.0, place.identifier.id.0, place.identifier.declaration_id.0
                    );
                }
                return read(constants, place);
            }
            let result = read(constants, place)?;
            // Preserve explicit receiver/object temporaries used by side-effecting
            // member operations. Upstream keeps these aliases in nested closures.
            if ctx
                .mutation_receiver_ids
                .contains(&instr.lvalue.identifier.id)
            {
                if debug_cp_trace_enabled() {
                    eprintln!(
                        "[CP_TRACE] skip LoadLocal fold due to mutation-receiver instr={} load_id={} place_id={} place_decl={}",
                        instr.id.0,
                        instr.lvalue.identifier.id.0,
                        place.identifier.id.0,
                        place.identifier.declaration_id.0
                    );
                }
                return Some(result);
            }
            // Don't rewrite LoadLocal if its result feeds a PrefixUpdate/PostfixUpdate —
            // codegen needs the variable reference (e.g., `i++` not `0++`).
            if ctx.update_value_ids.contains(&instr.lvalue.identifier.id) {
                return Some(result);
            }
            // Loop-carried variables must not fold through loads either. The
            // single-pass analysis does not model back-edges, so retaining the
            // initializer constant here can incorrectly collapse tests like
            // `while (i < 10)` to `while (true)` and freeze uses such as `key={i}`.
            if place.identifier.name.is_some()
                && ctx
                    .for_loop_var_decl_ids
                    .contains(&place.identifier.declaration_id)
            {
                return None;
            }
            // Don't rewrite LoadLocal for variables reassigned multiple times —
            // our HIR lowers logical/ternary as flat instructions (both sides
            // eagerly evaluated), so the last StoreLocal wins incorrectly.
            if place.identifier.name.is_some()
                && ctx
                    .multi_reassign_decl_ids
                    .contains(&place.identifier.declaration_id)
            {
                return None;
            }
            // Replace the LoadLocal with the constant value
            match &result {
                Constant::Primitive(pv, loc) => {
                    let instr_id = instr.id.0;
                    let load_id = instr.lvalue.identifier.id.0;
                    let load_decl = instr.lvalue.identifier.declaration_id.0;
                    let place_id = place.identifier.id.0;
                    let place_decl = place.identifier.declaration_id.0;
                    instr.value = InstructionValue::Primitive {
                        value: pv.clone(),
                        loc: loc.clone(),
                    };
                    if debug_cp_trace_enabled() {
                        eprintln!(
                            "[CP_TRACE] fold LoadLocal instr={} load_id={} load_decl={} place_id={} place_decl={} => Primitive",
                            instr_id, load_id, load_decl, place_id, place_decl
                        );
                    }
                }
                Constant::LoadGlobal(binding, loc) => {
                    let instr_id = instr.id.0;
                    let load_id = instr.lvalue.identifier.id.0;
                    let load_decl = instr.lvalue.identifier.declaration_id.0;
                    let place_id = place.identifier.id.0;
                    let place_decl = place.identifier.declaration_id.0;
                    instr.value = InstructionValue::LoadGlobal {
                        binding: binding.clone(),
                        loc: loc.clone(),
                    };
                    if debug_cp_trace_enabled() {
                        eprintln!(
                            "[CP_TRACE] fold LoadLocal instr={} load_id={} load_decl={} place_id={} place_decl={} => LoadGlobal({})",
                            instr_id,
                            load_id,
                            load_decl,
                            place_id,
                            place_decl,
                            non_local_binding_name(binding)
                        );
                    }
                }
            }
            Some(result)
        }
        InstructionValue::LoadContext { place, .. } => {
            let result = read(constants, place)?;
            let place_id = place.identifier.id.0;
            let place_decl = place.identifier.declaration_id.0;
            let load_id = instr.lvalue.identifier.id.0;
            let load_decl = instr.lvalue.identifier.declaration_id.0;
            match &result {
                Constant::Primitive(pv, loc) => {
                    instr.value = InstructionValue::Primitive {
                        value: pv.clone(),
                        loc: loc.clone(),
                    };
                    if debug_cp_trace_enabled() {
                        eprintln!(
                            "[CP_TRACE] fold LoadContext instr={} load_id={} load_decl={} place_id={} place_decl={} => Primitive",
                            instr.id.0, load_id, load_decl, place_id, place_decl
                        );
                    }
                    Some(result)
                }
                Constant::LoadGlobal(_, _) => {
                    let Constant::LoadGlobal(binding, loc) = &result else {
                        unreachable!();
                    };
                    instr.value = InstructionValue::LoadGlobal {
                        binding: binding.clone(),
                        loc: loc.clone(),
                    };
                    if debug_cp_trace_enabled() {
                        eprintln!(
                            "[CP_TRACE] fold LoadContext instr={} load_id={} load_decl={} place_id={} place_decl={} => LoadGlobal({})",
                            instr.id.0,
                            load_id,
                            load_decl,
                            place_id,
                            place_decl,
                            non_local_binding_name(binding)
                        );
                    }
                    Some(result)
                }
            }
        }
        InstructionValue::StoreLocal { value, lvalue, .. } => {
            if lvalue.place.identifier.name.is_some()
                && ctx
                    .mutated_captured_decl_ids
                    .contains(&lvalue.place.identifier.declaration_id)
            {
                return None;
            }
            let place_value = read(constants, value)?;
            // Keep mutable alias variables when initialized from a different
            // non-local name (e.g. `let logLevel = level; ...; logLevel = ...`).
            // Upstream's context lowering prevents this from over-folding.
            if let Constant::LoadGlobal(NonLocalBinding::Global { name }, _) = &place_value
                && ctx
                    .reassigned_decl_ids
                    .contains(&lvalue.place.identifier.declaration_id)
                && lvalue
                    .place
                    .identifier
                    .name
                    .as_ref()
                    .is_some_and(|local| local.value() != name)
            {
                if debug_cp_trace_enabled() {
                    eprintln!(
                        "[CP_TRACE] skip StoreLocal alias propagation instr={} target_decl={} local_name={:?} source_name={}",
                        instr.id.0,
                        lvalue.place.identifier.declaration_id.0,
                        lvalue
                            .place
                            .identifier
                            .name
                            .as_ref()
                            .map(|n| n.value().to_string()),
                        name
                    );
                }
                return None;
            }
            // Don't propagate for-loop init variables — their values change
            // across iterations due to the update clause back-edge.
            if lvalue.place.identifier.name.is_some()
                && ctx
                    .for_loop_var_decl_ids
                    .contains(&lvalue.place.identifier.declaration_id)
            {
                return None;
            }
            constants.insert(lvalue.place.identifier.id, place_value.clone());
            if debug_cp_trace_enabled() {
                eprintln!(
                    "[CP_TRACE] propagate StoreLocal instr={} target_id={} target_decl={} value_id={} value_decl={} const={:?}",
                    instr.id.0,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    value.identifier.id.0,
                    value.identifier.declaration_id.0,
                    place_value
                );
            }
            Some(place_value)
        }
        InstructionValue::StoreContext { value, lvalue, .. } => {
            if debug_cp_trace_enabled() {
                let known_value = read(constants, value).is_some();
                eprintln!(
                    "[CP_TRACE] observe StoreContext instr={} target_id={} target_decl={} value_id={} value_decl={} value_is_constant={}",
                    instr.id.0,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    value.identifier.id.0,
                    value.identifier.declaration_id.0,
                    known_value
                );
            }
            None
        }
        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            // Preserve the original ternary expression shape for codegen parity,
            // but still propagate a constant value when both branches are equal.
            let consequent_const = read(constants, consequent)?;
            let alternate_const = read(constants, alternate)?;
            if constants_equal(&consequent_const, &alternate_const) {
                if debug_cp_trace_enabled() {
                    eprintln!(
                        "[CP_TRACE] infer Ternary constant instr={} lvalue_id={} lvalue_decl={}",
                        instr.id.0,
                        instr.lvalue.identifier.id.0,
                        instr.lvalue.identifier.declaration_id.0
                    );
                }
                Some(consequent_const)
            } else {
                None
            }
        }
        InstructionValue::BinaryExpression {
            operator,
            left,
            right,
            loc,
        } => {
            let lhs_const = read(constants, left)?;
            let rhs_const = read(constants, right)?;
            let (Constant::Primitive(lhs, _), Constant::Primitive(rhs, _)) =
                (&lhs_const, &rhs_const)
            else {
                return None;
            };
            let loc = loc.clone();
            let operator = *operator;
            let result = evaluate_binary_op(operator, lhs, rhs)?;
            instr.value = InstructionValue::Primitive {
                value: result.clone(),
                loc: loc.clone(),
            };
            Some(Constant::Primitive(result, loc))
        }
        InstructionValue::UnaryExpression {
            operator,
            value,
            loc,
        } => {
            let operand = read(constants, value)?;
            let Constant::Primitive(pv, _) = &operand else {
                return None;
            };
            let loc = loc.clone();
            let operator = *operator;
            let result = evaluate_unary_op(operator, pv)?;
            instr.value = InstructionValue::Primitive {
                value: result.clone(),
                loc: loc.clone(),
            };
            Some(Constant::Primitive(result, loc))
        }
        InstructionValue::ComputedLoad {
            object,
            property,
            loc,
            optional,
        } => {
            // If property is a known constant string/number, convert to PropertyLoad
            let prop_const = read(constants, property)?;
            if let Constant::Primitive(pv, _) = &prop_const {
                match pv {
                    PrimitiveValue::String(s) if is_valid_identifier(s) => {
                        let loc = loc.clone();
                        let optional = *optional;
                        let object = object.clone();
                        instr.value = InstructionValue::PropertyLoad {
                            object,
                            property: PropertyLiteral::String(s.clone()),
                            optional,
                            loc,
                        };
                    }
                    PrimitiveValue::Number(n) if *n >= 0.0 && n.fract() == 0.0 => {
                        let loc = loc.clone();
                        let optional = *optional;
                        let object = object.clone();
                        instr.value = InstructionValue::PropertyLoad {
                            object,
                            property: PropertyLiteral::Number(*n),
                            optional,
                            loc,
                        };
                    }
                    _ => {}
                }
            }
            None
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            loc,
        } => {
            let prop_const = read(constants, property)?;
            if let Constant::Primitive(pv, _) = &prop_const {
                match pv {
                    PrimitiveValue::String(s) if is_valid_identifier(s) => {
                        let loc = loc.clone();
                        let object = object.clone();
                        let value = value.clone();
                        instr.value = InstructionValue::PropertyStore {
                            object,
                            property: PropertyLiteral::String(s.clone()),
                            value,
                            loc,
                        };
                    }
                    PrimitiveValue::Number(n) if *n >= 0.0 && n.fract() == 0.0 => {
                        let loc = loc.clone();
                        let object = object.clone();
                        let value = value.clone();
                        instr.value = InstructionValue::PropertyStore {
                            object,
                            property: PropertyLiteral::Number(*n),
                            value,
                            loc,
                        };
                    }
                    _ => {}
                }
            }
            None
        }
        InstructionValue::PrefixUpdate {
            lvalue,
            operation,
            value,
            loc,
        } => {
            let prev = read(constants, value)?;
            if let Constant::Primitive(PrimitiveValue::Number(n), _) = &prev {
                let n = *n;
                let loc = loc.clone();
                let next = match operation {
                    UpdateOperator::Increment => n + 1.0,
                    UpdateOperator::Decrement => n - 1.0,
                };
                let result = PrimitiveValue::Number(next);
                let new_const = Constant::Primitive(result.clone(), loc.clone());
                constants.insert(lvalue.identifier.id, new_const.clone());
                // Propagate back to the named variable so subsequent LoadLocals see it
                if let Some(&named_var_id) = ctx.load_local_source.get(&value.identifier.id) {
                    constants.insert(named_var_id, new_const);
                }
                return Some(Constant::Primitive(result, loc));
            }
            None
        }
        InstructionValue::PostfixUpdate {
            lvalue,
            operation,
            value,
            loc,
        } => {
            let prev = read(constants, value)?;
            if let Constant::Primitive(PrimitiveValue::Number(n), _) = &prev {
                let n = *n;
                let loc = loc.clone();
                let next = match operation {
                    UpdateOperator::Increment => n + 1.0,
                    UpdateOperator::Decrement => n - 1.0,
                };
                let new_const = Constant::Primitive(PrimitiveValue::Number(next), loc);
                // Store the updated value for the lvalue
                constants.insert(lvalue.identifier.id, new_const.clone());
                // Propagate back to the named variable so subsequent LoadLocals see it
                if let Some(&named_var_id) = ctx.load_local_source.get(&value.identifier.id) {
                    constants.insert(named_var_id, new_const);
                }
                // But return the value PRIOR to the update
                return Some(prev);
            }
            None
        }
        InstructionValue::PropertyLoad {
            object, property, ..
        } => {
            // string.length folding
            let obj_const = read(constants, object)?;
            if let Constant::Primitive(PrimitiveValue::String(s), _) = &obj_const
                && matches!(property, PropertyLiteral::String(p) if p == "length")
            {
                let loc = instr.value.loc().clone();
                let result = PrimitiveValue::Number(s.len() as f64);
                instr.value = InstructionValue::Primitive {
                    value: result.clone(),
                    loc: loc.clone(),
                };
                return Some(Constant::Primitive(result, loc));
            }
            None
        }
        InstructionValue::TemplateLiteral {
            subexprs,
            quasis,
            loc,
        } => {
            if subexprs.is_empty() {
                // No interpolations — fold to joined quasis
                let joined: Option<String> = quasis
                    .iter()
                    .map(|q| q.cooked.as_deref())
                    .collect::<Option<Vec<_>>>()
                    .map(|parts| parts.join(""));
                if let Some(s) = joined {
                    let loc = loc.clone();
                    let result = PrimitiveValue::String(s);
                    instr.value = InstructionValue::Primitive {
                        value: result.clone(),
                        loc: loc.clone(),
                    };
                    return Some(Constant::Primitive(result, loc));
                }
                return None;
            }

            // Verify invariant: subexprs.len() == quasis.len() - 1
            if subexprs.len() != quasis.len() - 1 {
                return None;
            }

            // Check all quasis have cooked values
            if quasis.iter().any(|q| q.cooked.is_none()) {
                return None;
            }

            // Build result string by interleaving quasis and subexpr values
            let mut result_string = quasis[0].cooked.clone().unwrap();
            for (i, subexpr) in subexprs.iter().enumerate() {
                let sub_const = read(constants, subexpr)?;
                let Constant::Primitive(pv, _) = &sub_const else {
                    return None;
                };
                // Only fold number, string, boolean, null — NOT undefined
                // (matches upstream which checks typeof !== undefined)
                if matches!(pv, PrimitiveValue::Undefined) {
                    return None;
                }
                result_string.push_str(&primitive_to_string(pv));
                if let Some(suffix) = &quasis[i + 1].cooked {
                    result_string.push_str(suffix);
                } else {
                    return None;
                }
            }

            let loc = loc.clone();
            let result = PrimitiveValue::String(result_string);
            instr.value = InstructionValue::Primitive {
                value: result.clone(),
                loc: loc.clone(),
            };
            Some(Constant::Primitive(result, loc))
        }
        _ => None,
    }
}

fn read(constants: &Constants, place: &Place) -> Option<Constant> {
    constants.get(&place.identifier.id).cloned()
}

fn constants_equal(lhs: &Constant, rhs: &Constant) -> bool {
    match (lhs, rhs) {
        (Constant::LoadGlobal(lb, _), Constant::LoadGlobal(rb, _)) => {
            non_local_binding_name(lb) == non_local_binding_name(rb)
        }
        _ => false,
    }
}

fn primitive_eq(a: &PrimitiveValue, b: &PrimitiveValue) -> bool {
    match (a, b) {
        (PrimitiveValue::Null, PrimitiveValue::Null) => true,
        (PrimitiveValue::Undefined, PrimitiveValue::Undefined) => true,
        (PrimitiveValue::Boolean(a), PrimitiveValue::Boolean(b)) => a == b,
        (PrimitiveValue::Number(a), PrimitiveValue::Number(b)) => a == b,
        (PrimitiveValue::String(a), PrimitiveValue::String(b)) => a == b,
        _ => false,
    }
}

fn evaluate_binary_op(
    op: BinaryOperator,
    lhs: &PrimitiveValue,
    rhs: &PrimitiveValue,
) -> Option<PrimitiveValue> {
    match (lhs, rhs) {
        (PrimitiveValue::Number(l), PrimitiveValue::Number(r)) => {
            let l = *l;
            let r = *r;
            match op {
                BinaryOperator::Add => Some(PrimitiveValue::Number(l + r)),
                BinaryOperator::Sub => Some(PrimitiveValue::Number(l - r)),
                BinaryOperator::Mul => Some(PrimitiveValue::Number(l * r)),
                BinaryOperator::Div => Some(PrimitiveValue::Number(l / r)),
                BinaryOperator::Mod => Some(PrimitiveValue::Number(l % r)),
                BinaryOperator::Exp => Some(PrimitiveValue::Number(l.powf(r))),
                BinaryOperator::Lt => Some(PrimitiveValue::Boolean(l < r)),
                BinaryOperator::LtEq => Some(PrimitiveValue::Boolean(l <= r)),
                BinaryOperator::Gt => Some(PrimitiveValue::Boolean(l > r)),
                BinaryOperator::GtEq => Some(PrimitiveValue::Boolean(l >= r)),
                BinaryOperator::BitAnd => {
                    Some(PrimitiveValue::Number(((l as i64) & (r as i64)) as f64))
                }
                BinaryOperator::BitOr => {
                    Some(PrimitiveValue::Number(((l as i64) | (r as i64)) as f64))
                }
                BinaryOperator::BitXor => {
                    Some(PrimitiveValue::Number(((l as i64) ^ (r as i64)) as f64))
                }
                BinaryOperator::LShift => {
                    Some(PrimitiveValue::Number(((l as i32) << (r as u32)) as f64))
                }
                BinaryOperator::RShift => {
                    Some(PrimitiveValue::Number(((l as i32) >> (r as u32)) as f64))
                }
                BinaryOperator::URShift => {
                    Some(PrimitiveValue::Number(((l as u32) >> (r as u32)) as f64))
                }
                BinaryOperator::StrictEq => Some(PrimitiveValue::Boolean(l == r)),
                BinaryOperator::StrictNotEq => Some(PrimitiveValue::Boolean(l != r)),
                BinaryOperator::Eq => Some(PrimitiveValue::Boolean(l == r)),
                BinaryOperator::NotEq => Some(PrimitiveValue::Boolean(l != r)),
                _ => None,
            }
        }
        (PrimitiveValue::String(l), PrimitiveValue::String(r)) => match op {
            BinaryOperator::Add => Some(PrimitiveValue::String(format!("{}{}", l, r))),
            BinaryOperator::StrictEq | BinaryOperator::Eq => Some(PrimitiveValue::Boolean(l == r)),
            BinaryOperator::StrictNotEq | BinaryOperator::NotEq => {
                Some(PrimitiveValue::Boolean(l != r))
            }
            _ => None,
        },
        // Loose equality between different types
        (PrimitiveValue::Null, PrimitiveValue::Undefined)
        | (PrimitiveValue::Undefined, PrimitiveValue::Null) => match op {
            BinaryOperator::Eq => Some(PrimitiveValue::Boolean(true)),
            BinaryOperator::NotEq => Some(PrimitiveValue::Boolean(false)),
            BinaryOperator::StrictEq => Some(PrimitiveValue::Boolean(false)),
            BinaryOperator::StrictNotEq => Some(PrimitiveValue::Boolean(true)),
            _ => None,
        },
        // Same primitive type — equality
        _ => match op {
            BinaryOperator::StrictEq | BinaryOperator::Eq => {
                Some(PrimitiveValue::Boolean(primitive_eq(lhs, rhs)))
            }
            BinaryOperator::StrictNotEq | BinaryOperator::NotEq => {
                Some(PrimitiveValue::Boolean(!primitive_eq(lhs, rhs)))
            }
            _ => None,
        },
    }
}

fn evaluate_unary_op(op: UnaryOperator, operand: &PrimitiveValue) -> Option<PrimitiveValue> {
    match op {
        UnaryOperator::Not => {
            // Don't fold !undefined — upstream keeps it as-is
            if matches!(operand, PrimitiveValue::Undefined) {
                return None;
            }
            let is_truthy = match operand {
                PrimitiveValue::Boolean(b) => *b,
                PrimitiveValue::Number(n) => *n != 0.0 && !n.is_nan(),
                PrimitiveValue::String(s) => !s.is_empty(),
                PrimitiveValue::Null => false,
                PrimitiveValue::Undefined => unreachable!(),
            };
            Some(PrimitiveValue::Boolean(!is_truthy))
        }
        UnaryOperator::Minus => match operand {
            PrimitiveValue::Number(n) => Some(PrimitiveValue::Number(-n)),
            _ => None,
        },
        // Upstream only folds ! and - for unary operators
        _ => None,
    }
}

/// Convert a PrimitiveValue to its JavaScript ToString representation.
/// Used for template literal folding (matches JS ToString semantics).
fn primitive_to_string(pv: &PrimitiveValue) -> String {
    match pv {
        PrimitiveValue::String(s) => s.clone(),
        PrimitiveValue::Number(n) => {
            if n.is_nan() {
                "NaN".to_string()
            } else if n.is_infinite() {
                if *n > 0.0 {
                    "Infinity".to_string()
                } else {
                    "-Infinity".to_string()
                }
            } else if *n == 0.0 {
                "0".to_string()
            } else {
                // Format without trailing .0 for integers
                let i = *n as i64;
                if (i as f64) == *n {
                    i.to_string()
                } else {
                    n.to_string()
                }
            }
        }
        PrimitiveValue::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
        PrimitiveValue::Null => "null".to_string(),
        PrimitiveValue::Undefined => "undefined".to_string(),
    }
}

fn non_local_binding_name(b: &NonLocalBinding) -> &str {
    match b {
        NonLocalBinding::ImportDefault { name, .. }
        | NonLocalBinding::ImportNamespace { name, .. }
        | NonLocalBinding::ImportSpecifier { name, .. }
        | NonLocalBinding::ModuleLocal { name }
        | NonLocalBinding::Global { name } => name,
    }
}

/// Check if a string is a valid JavaScript identifier.
fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}
