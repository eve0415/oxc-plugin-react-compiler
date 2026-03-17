//! Dead Code Elimination — remove unused instructions.
//!
//! Port of `DeadCodeElimination.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Removes instructions whose lvalue is never used as an operand
//! in any subsequent instruction or terminal. This handles:
//! - `let _ = <div a={a} />` where `_` is never read
//! - Unused temporaries

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::hir::builder::each_terminal_successor;
use crate::hir::types::*;
use crate::hir::visitors;

thread_local! {
    static PRESERVED_TOP_LEVEL_LET_INITIALIZERS: RefCell<HashMap<DeclarationId, String>> =
        RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, Default)]
struct DceOptions {
    /// Preserve unused user-named `DeclareLocal` instructions.
    ///
    /// This is used for the post-outline cleanup pass to avoid erasing
    /// declaration-shape artifacts that upstream keeps.
    preserve_unused_named_declare_locals: bool,
}

/// Remove dead code from a function.
///
/// An instruction is dead if its lvalue identifier is never read
/// as an operand in any instruction or terminal.
pub fn dead_code_elimination(func: &mut HIRFunction) {
    dead_code_elimination_with_options(func, DceOptions::default());
}

/// Post-outline cleanup variant.
///
/// Keep unused user-named declarations to match upstream declaration-shape
/// behavior while still pruning other dead instructions.
pub fn dead_code_elimination_post_outline(func: &mut HIRFunction) {
    dead_code_elimination_with_options(
        func,
        DceOptions {
            preserve_unused_named_declare_locals: true,
        },
    );
}

#[allow(dead_code)]
pub(crate) fn preserved_top_level_let_initializer_for_decl(
    decl_id: DeclarationId,
) -> Option<String> {
    PRESERVED_TOP_LEVEL_LET_INITIALIZERS.with(|slot| slot.borrow().get(&decl_id).cloned())
}

pub(crate) fn clear_preserved_top_level_let_initializers() {
    PRESERVED_TOP_LEVEL_LET_INITIALIZERS.with(|slot| slot.borrow_mut().clear());
}

fn dead_code_elimination_with_options(func: &mut HIRFunction, options: DceOptions) {
    let debug_dce_reassign = std::env::var("DEBUG_DCE_REASSIGN").is_ok();
    let debug_dce_rewrite = std::env::var("DEBUG_DCE_REWRITE").is_ok();
    // Keep DCE sweep semantics aligned with upstream:
    // referenced-id discovery already runs to loop-aware fixpoint, then prune once.
    dead_code_elimination_pass(func, debug_dce_reassign, debug_dce_rewrite, options);
}

/// Check if an identifier is used by ID or by name.
fn is_id_or_name_used(
    id: &Identifier,
    used_ids: &HashSet<IdentifierId>,
    used_names: &HashSet<String>,
) -> bool {
    if used_ids.contains(&id.id) {
        return true;
    }
    if let Some(name) = get_identifier_name(id)
        && used_names.contains(&name)
    {
        return true;
    }
    false
}

/// Check if an instruction value is pruneable (can be eliminated when unused).
/// Matches upstream `pruneableValue()` logic exactly.
/// `load_local_sources` maps temp IDs (from LoadLocal instructions) to the source
/// variable's Identifier, enabling PostfixUpdate/PrefixUpdate to check if the
/// variable they modify is used.
fn is_pruneable(
    value: &InstructionValue,
    used_ids: &HashSet<IdentifierId>,
    used_names: &HashSet<String>,
    load_local_sources: &HashMap<IdentifierId, Identifier>,
    local_decl_ids: &HashSet<DeclarationId>,
    protected_reassign_place_ids: &HashSet<IdentifierId>,
    captured_context_decl_ids: &HashSet<DeclarationId>,
) -> bool {
    match value {
        InstructionValue::DeclareLocal { lvalue, .. } => {
            // Declarations are pruneable only if the named variable is never read later
            !is_id_or_name_used(&lvalue.place.identifier, used_ids, used_names)
        }
        InstructionValue::StoreLocal { lvalue, .. } => {
            if lvalue.kind == InstructionKind::Reassign {
                if captured_context_decl_ids.contains(&lvalue.place.identifier.declaration_id) {
                    // Reassignments observed through nested-function context reads
                    // must be kept. Upstream lowers many of these through context.
                    return false;
                }
                if protected_reassign_place_ids.contains(&lvalue.place.identifier.id) {
                    return false;
                }
                // Reassignments to outer-scope or global variables are side effects
                // that must not be pruned.
                if !local_decl_ids.contains(&lvalue.place.identifier.declaration_id) {
                    return false;
                }
                // Upstream: Reassignments can be pruned if the specific instance
                // being assigned is never read (SSA ID only, not name).
                !used_ids.contains(&lvalue.place.identifier.id)
            } else {
                // Declarations are pruneable only if the named variable is never read later
                !is_id_or_name_used(&lvalue.place.identifier, used_ids, used_names)
            }
        }
        InstructionValue::Destructure { lvalue, .. } => {
            let mut any_id_or_name_used = false;
            let mut any_id_used = false;
            for place in pattern_places(&lvalue.pattern) {
                if used_ids.contains(&place.identifier.id) {
                    any_id_or_name_used = true;
                    any_id_used = true;
                } else if is_id_or_name_used(&place.identifier, used_ids, used_names) {
                    any_id_or_name_used = true;
                }
            }
            if lvalue.kind == InstructionKind::Reassign {
                !any_id_used
            } else {
                !any_id_or_name_used
            }
        }
        // Read-only operations: safe to prune
        InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::ObjectExpression { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::UnaryExpression { .. } => true,
        // Ternary and LogicalExpression may have side-effectful operands
        // (e.g. assignment expressions in branches). Keep them even if
        // the result is unused so the side effects are preserved.
        InstructionValue::Ternary { .. } | InstructionValue::LogicalExpression { .. } => false,
        InstructionValue::PostfixUpdate { value, .. }
        | InstructionValue::PrefixUpdate { value, .. } => {
            // PostfixUpdate/PrefixUpdate modify the underlying variable as a side effect.
            // The `value` field is a temp (from LoadLocal), so we trace back through
            // load_local_sources to find the original variable being modified.
            // Only check by SSA ID (not name) to allow pruning when the specific
            // SSA version is dead even though another version of the same variable lives.
            if let Some(source_ident) = load_local_sources.get(&value.identifier.id) {
                !used_ids.contains(&source_ident.id)
            } else {
                true // Can't determine source — default to pruneable
            }
        }
        // Non-pruneable: side-effectful, iterator, context, or memoization ops
        InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::IteratorNext { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. }
        | InstructionValue::FinishMemoize { .. } => false,
    }
}

/// Non-throwing operations we can safely treat as a guaranteed prefix within
/// a try block when deciding whether a pre-try initializer is dead.
fn is_non_throwing_try_prefix_instruction(instr: &Instruction) -> bool {
    matches!(
        instr.value,
        InstructionValue::Primitive { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
            | InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
    )
}

/// For each block that ends with a Try terminal, collect declaration IDs that
/// are reassigned in the try-entry block before the first potentially-throwing
/// instruction. These declarations can safely drop pre-try initializers.
fn collect_try_prefix_reassigned_decls(
    func: &HIRFunction,
) -> HashMap<BlockId, HashSet<DeclarationId>> {
    let block_map: HashMap<BlockId, &BasicBlock> =
        func.body.blocks.iter().map(|(id, b)| (*id, b)).collect();
    let mut result: HashMap<BlockId, HashSet<DeclarationId>> = HashMap::new();

    for (_, parent) in &func.body.blocks {
        let Terminal::Try {
            block: try_block, ..
        } = &parent.terminal
        else {
            continue;
        };
        let Some(try_entry) = block_map.get(try_block) else {
            continue;
        };

        let mut reassigned: HashSet<DeclarationId> = HashSet::new();
        for instr in &try_entry.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } = &instr.value
                && lvalue.kind == InstructionKind::Reassign
            {
                reassigned.insert(lvalue.place.identifier.declaration_id);
            }

            if !is_non_throwing_try_prefix_instruction(instr) {
                break;
            }
        }

        if !reassigned.is_empty() {
            result.insert(parent.id, reassigned);
        }
    }

    result
}

fn terminal_immediate_successors(terminal: &Terminal) -> Vec<BlockId> {
    match terminal {
        Terminal::Switch {
            cases, fallthrough, ..
        } => {
            let mut successors: Vec<BlockId> = cases.iter().map(|case_| case_.block).collect();
            successors.push(*fallthrough);
            successors
        }
        _ => each_terminal_successor(terminal),
    }
}

fn instruction_reads_declaration(instr: &Instruction, decl_id: DeclarationId) -> bool {
    let mut reads_decl = false;
    visitors::for_each_instruction_value_operand(&instr.value, |place| {
        if place.identifier.declaration_id == decl_id {
            reads_decl = true;
        }
    });
    reads_decl
}

fn instruction_writes_declaration(instr: &Instruction, decl_id: DeclarationId) -> bool {
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            lvalue.place.identifier.declaration_id == decl_id
        }
        InstructionValue::Destructure { lvalue, .. } => pattern_places(&lvalue.pattern)
            .into_iter()
            .any(|place| place.identifier.declaration_id == decl_id),
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            lvalue.identifier.declaration_id == decl_id
        }
        _ => false,
    }
}

fn terminal_reads_declaration(terminal: &Terminal, decl_id: DeclarationId) -> bool {
    let mut reads_decl = false;
    visitors::for_each_terminal_operand(terminal, |place| {
        if place.identifier.declaration_id == decl_id {
            reads_decl = true;
        }
    });
    reads_decl
}

fn scan_block_for_reassign_before_read(
    block: &BasicBlock,
    start_idx: usize,
    decl_id: DeclarationId,
    mut reassigned: bool,
) -> Option<bool> {
    for instr in block.instructions.iter().skip(start_idx) {
        if !reassigned && instruction_reads_declaration(instr, decl_id) {
            return None;
        }
        if instruction_writes_declaration(instr, decl_id) {
            reassigned = true;
        }
    }

    if !reassigned && terminal_reads_declaration(&block.terminal, decl_id) {
        return None;
    }

    Some(reassigned)
}

fn top_level_literal_initializer_is_elidable(
    func: &HIRFunction,
    block_map: &HashMap<BlockId, &BasicBlock>,
    entry_instr_index: usize,
    decl_id: DeclarationId,
) -> bool {
    let Some(entry_block) = block_map.get(&func.body.entry).copied() else {
        return false;
    };

    let Some(initial_state) =
        scan_block_for_reassign_before_read(entry_block, entry_instr_index + 1, decl_id, false)
    else {
        return false;
    };

    let mut queue: VecDeque<(BlockId, bool)> = terminal_immediate_successors(&entry_block.terminal)
        .into_iter()
        .map(|block_id| (block_id, initial_state))
        .collect();
    let mut seen_false: HashSet<BlockId> = HashSet::new();
    let mut seen_true: HashSet<BlockId> = HashSet::new();

    while let Some((block_id, reassigned)) = queue.pop_front() {
        let seen = if reassigned {
            &mut seen_true
        } else {
            &mut seen_false
        };
        if !seen.insert(block_id) {
            continue;
        }

        let Some(block) = block_map.get(&block_id).copied() else {
            return false;
        };
        let Some(next_state) = scan_block_for_reassign_before_read(block, 0, decl_id, reassigned)
        else {
            return false;
        };

        for succ in terminal_immediate_successors(&block.terminal) {
            queue.push_back((succ, next_state));
        }
    }

    true
}

fn primitive_to_js(value: &PrimitiveValue) -> String {
    match value {
        PrimitiveValue::Null => "null".to_string(),
        PrimitiveValue::Undefined => "undefined".to_string(),
        PrimitiveValue::Boolean(value) => value.to_string(),
        PrimitiveValue::Number(value) => {
            if *value == 0.0 && value.is_sign_negative() {
                "0".to_string()
            } else if *value < 0.0 {
                format!("-{}", -*value)
            } else {
                value.to_string()
            }
        }
        PrimitiveValue::String(value) => format!("{value:?}"),
    }
}

fn dead_code_elimination_pass(
    func: &mut HIRFunction,
    debug_dce_reassign: bool,
    debug_dce_rewrite: bool,
    options: DceOptions,
) {
    // Phase 1: Find all referenced identifiers using backward analysis.
    // This matches upstream's findReferencedIdentifiers():
    // - Start with terminal operands as seeds
    // - For each instruction, only mark operands as used if the instruction
    //   itself is used (or non-pruneable)
    // - For StoreLocal declarations, only mark the value operand if the
    //   lvalue.place's specific SSA ID is used (not just the name)
    // - Iterate to fixpoint for loops

    let mut used_ids: HashSet<IdentifierId> = HashSet::new();
    let mut used_names: HashSet<String> = HashSet::new();

    // Build set of declaration IDs that originate in this function.
    // Variables whose declaration_id is NOT in this set are captured from
    // an outer scope — reassignments to them are side effects.
    let mut local_decl_ids: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                    if lvalue.kind != InstructionKind::Reassign =>
                {
                    local_decl_ids.insert(lvalue.place.identifier.declaration_id);
                }
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    local_decl_ids.insert(lvalue.place.identifier.declaration_id);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    for place in pattern_places(&lvalue.pattern) {
                        local_decl_ids.insert(place.identifier.declaration_id);
                    }
                }
                _ => {}
            }
        }
    }
    // Also add parameter declaration IDs
    for param in &func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        local_decl_ids.insert(place.identifier.declaration_id);
    }

    // Captured locals should behave like context variables for pruning.
    // For `effect=Read` captures, only protect when the captured declaration is
    // reassigned in the enclosing function. Otherwise we over-preserve dead stores.
    fn collect_captured_context_decl_ids(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
        let mut reassigned_decl_ids: HashSet<DeclarationId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                    | InstructionValue::StoreContext { lvalue, .. }
                        if lvalue.kind == InstructionKind::Reassign =>
                    {
                        reassigned_decl_ids.insert(lvalue.place.identifier.declaration_id);
                    }
                    _ => {}
                }
            }
        }
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        for operand in &lowered_func.func.context {
                            let protect_decl = operand.effect == Effect::Capture
                                || (operand.effect == Effect::Read
                                    && reassigned_decl_ids
                                        .contains(&operand.identifier.declaration_id));
                            if protect_decl {
                                out.insert(operand.identifier.declaration_id);
                            }
                        }
                        collect_captured_context_decl_ids(&lowered_func.func, out);
                    }
                    _ => {}
                }
            }
        }
    }
    let mut captured_context_decl_ids: HashSet<DeclarationId> = HashSet::new();
    collect_captured_context_decl_ids(func, &mut captured_context_decl_ids);

    // Build map: temp ID → source variable Identifier (from LoadLocal instructions).
    // This allows PostfixUpdate/PrefixUpdate to trace back to the variable they modify.
    let mut load_local_sources: HashMap<IdentifierId, Identifier> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadLocal { place, .. } = &instr.value
                && place.identifier.name.is_some()
            {
                load_local_sources.insert(instr.lvalue.identifier.id, place.identifier.clone());
            }
        }
    }

    // Track StartMemoize deps to preserve post-marker reassignments.
    // Without this, DCE can erase a later mutation and hide a required
    // preserve-memo bailout.
    let mut first_start_memo_instr_by_decl: HashMap<DeclarationId, InstructionId> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StartMemoize {
                deps: Some(deps), ..
            } = &instr.value
            {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(place) = &dep.root {
                        first_start_memo_instr_by_decl
                            .entry(place.identifier.declaration_id)
                            .and_modify(|first| {
                                if instr.id < *first {
                                    *first = instr.id;
                                }
                            })
                            .or_insert(instr.id);
                    }
                }
            }
        }
    }

    let mut protected_reassign_place_ids: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                && lvalue.kind == InstructionKind::Reassign
                && let Some(start_id) =
                    first_start_memo_instr_by_decl.get(&lvalue.place.identifier.declaration_id)
                && instr.id > *start_id
            {
                protected_reassign_place_ids.insert(lvalue.place.identifier.id);
                if debug_dce_reassign {
                    let name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => "<unnamed>",
                    };
                    eprintln!(
                        "[DCE_REASSIGN] protect bb{} instr#{} name={} id={} decl={} start_memo_instr#{}",
                        block.id.0,
                        instr.id.0,
                        name,
                        lvalue.place.identifier.id.0,
                        lvalue.place.identifier.declaration_id.0,
                        start_id.0
                    );
                }
            }
        }
    }

    // Helper: reference an identifier (mark by ID and name)
    fn reference(
        id: &Identifier,
        used_ids: &mut HashSet<IdentifierId>,
        used_names: &mut HashSet<String>,
    ) {
        used_ids.insert(id.id);
        if let Some(name) = get_identifier_name(id) {
            used_names.insert(name);
        }
    }

    // Collect blocks in reverse order for backward analysis
    let block_ids: Vec<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();
    let reversed_block_ids: Vec<BlockId> = block_ids.into_iter().rev().collect();

    // Fixpoint iteration
    loop {
        let prev_count = used_ids.len() + used_names.len();

        for &block_id in &reversed_block_ids {
            let block = match func.body.blocks.iter().find(|(id, _)| *id == block_id) {
                Some((_, b)) => b,
                None => continue,
            };

            // Terminal operands are always live
            collect_terminal_used(&block.terminal, &mut used_ids);

            // Process instructions in reverse
            for i in (0..block.instructions.len()).rev() {
                let instr = &block.instructions[i];
                let is_block_value =
                    block.kind != BlockKind::Block && i == block.instructions.len() - 1;

                if is_block_value {
                    // Last instruction of a value block — always keep, reference all operands
                    reference(&instr.lvalue.identifier, &mut used_ids, &mut used_names);
                    visitors::for_each_instruction_operand(instr, |place| {
                        reference(&place.identifier, &mut used_ids, &mut used_names);
                    });
                } else if is_id_or_name_used(&instr.lvalue.identifier, &used_ids, &used_names)
                    || !is_pruneable(
                        &instr.value,
                        &used_ids,
                        &used_names,
                        &load_local_sources,
                        &local_decl_ids,
                        &protected_reassign_place_ids,
                        &captured_context_decl_ids,
                    )
                {
                    // Instruction is used or non-pruneable — mark it and its operands
                    reference(&instr.lvalue.identifier, &mut used_ids, &mut used_names);

                    // Special handling for StoreLocal: only reference the value
                    // if the lvalue is a Reassign or if the lvalue.place SSA ID
                    // is directly used. This matches upstream's approach where
                    // a Let/Const declaration's initializer is only kept alive
                    // when the specific SSA version is read, enabling dead store
                    // elimination for overwritten initializers.
                    if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                        if lvalue.kind == InstructionKind::Reassign
                            || used_ids.contains(&lvalue.place.identifier.id)
                        {
                            reference(&value.identifier, &mut used_ids, &mut used_names);
                        }
                    } else {
                        // For all other instructions, reference all operands
                        visitors::for_each_instruction_operand(instr, |place| {
                            reference(&place.identifier, &mut used_ids, &mut used_names);
                        });
                    }
                }
            }

            // Process phi nodes
            for phi in &block.phis {
                if is_id_or_name_used(&phi.place.identifier, &used_ids, &used_names) {
                    for operand in phi.operands.values() {
                        reference(&operand.identifier, &mut used_ids, &mut used_names);
                    }
                }
            }
        }

        let new_count = used_ids.len() + used_names.len();
        if new_count == prev_count {
            break;
        }
    }

    // Phase 2: Rewrite and prune

    let try_prefix_reassigned_decls = collect_try_prefix_reassigned_decls(func);
    let block_map: HashMap<BlockId, &BasicBlock> = func
        .body
        .blocks
        .iter()
        .map(|(id, block)| (*id, block))
        .collect();
    let value_defs: HashMap<IdentifierId, &InstructionValue> = func
        .body
        .blocks
        .iter()
        .flat_map(|(_, block)| {
            block
                .instructions
                .iter()
                .map(|instr| (instr.lvalue.identifier.id, &instr.value))
        })
        .collect();

    // Rewrite StoreLocal → DeclareLocal when the initializer value is dead
    // but the variable binding is still needed.
    // Matching upstream rewriteInstruction(): convert when:
    // 1. Not a Reassign
    // 2. lvalue.place SSA ID is NOT in used_ids (the specific version is not read)
    // The variable may still be needed (alive by name), but the initializer is dead
    // because the variable is always overwritten before being read.
    let mut rewritten_to_declare_instr_ids: HashSet<InstructionId> = HashSet::new();
    let mut rewritten_rhs_ids: HashMap<InstructionId, IdentifierId> = HashMap::new();
    let mut preserved_initializers: HashMap<DeclarationId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        let num_instrs = block.instructions.len();
        for (idx, instr) in block.instructions.iter().enumerate() {
            let is_block_value = block.kind != BlockKind::Block && idx == num_instrs - 1;
            if is_block_value {
                // Upstream does not rewrite the terminal value instruction for value blocks.
                continue;
            }
            if debug_dce_rewrite && let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                let force_rewrite_for_try = try_prefix_reassigned_decls
                    .get(&block.id)
                    .is_some_and(|decls| decls.contains(&lvalue.place.identifier.declaration_id));
                let name = match &lvalue.place.identifier.name {
                    Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                        n.as_str()
                    }
                    None => "<unnamed>",
                };
                eprintln!(
                    "[DCE_REWRITE] inspect bb{} instr#{} name={} kind={:?} place_id={} decl={} used_place_id={} force_try={}",
                    block.id.0,
                    instr.id.0,
                    name,
                    lvalue.kind,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    used_ids.contains(&lvalue.place.identifier.id),
                    force_rewrite_for_try,
                );
            }
            let force_rewrite_for_try = match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. } => try_prefix_reassigned_decls
                    .get(&block.id)
                    .is_some_and(|decls| decls.contains(&lvalue.place.identifier.declaration_id)),
                _ => false,
            };
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                && lvalue.kind != InstructionKind::Reassign
            {
                let is_entry_named_let = block.id == func.body.entry
                    && lvalue.kind == InstructionKind::Let
                    && lvalue.place.identifier.name.is_some()
                    && !captured_context_decl_ids.contains(&lvalue.place.identifier.declaration_id);
                let primitive_literal = is_entry_named_let
                    .then(|| match value_defs.get(&value.identifier.id) {
                        Some(InstructionValue::Primitive { value, .. }) => Some(value),
                        _ => None,
                    })
                    .flatten();
                let precise_literal_let_rewrite = primitive_literal.map(|_| {
                    top_level_literal_initializer_is_elidable(
                        func,
                        &block_map,
                        idx,
                        lvalue.place.identifier.declaration_id,
                    )
                });
                let should_rewrite = if force_rewrite_for_try {
                    true
                } else if let Some(is_elidable) = precise_literal_let_rewrite {
                    is_elidable
                } else if is_entry_named_let {
                    // For user-named let declarations in the entry block whose
                    // primitive value lookup failed, use BFS elidable check.
                    top_level_literal_initializer_is_elidable(
                        func,
                        &block_map,
                        idx,
                        lvalue.place.identifier.declaration_id,
                    )
                } else {
                    !used_ids.contains(&lvalue.place.identifier.id)
                };
                if !force_rewrite_for_try
                    && let (Some(false), Some(primitive)) =
                        (precise_literal_let_rewrite, primitive_literal)
                {
                    preserved_initializers.insert(
                        lvalue.place.identifier.declaration_id,
                        primitive_to_js(primitive),
                    );
                }
                if !should_rewrite {
                    continue;
                }
                if debug_dce_rewrite {
                    let name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => "<unnamed>",
                    };
                    eprintln!(
                        "[DCE_REWRITE] rewrite bb{} instr#{} name={} place_id={} decl={} rhs_id={} force_try={}",
                        block.id.0,
                        instr.id.0,
                        name,
                        lvalue.place.identifier.id.0,
                        lvalue.place.identifier.declaration_id.0,
                        value.identifier.id.0,
                        force_rewrite_for_try,
                    );
                }
                rewritten_rhs_ids.insert(instr.id, value.identifier.id);
                rewritten_to_declare_instr_ids.insert(instr.id);
            }
        }
    }

    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if !rewritten_to_declare_instr_ids.contains(&instr.id) {
                continue;
            }
            let InstructionValue::StoreLocal { lvalue, .. } = &instr.value else {
                continue;
            };
            let new_lvalue = lvalue.clone();
            let loc = instr.value.loc().clone();
            instr.value = InstructionValue::DeclareLocal {
                lvalue: new_lvalue,
                loc,
            };
            if let Some(rhs_id) = rewritten_rhs_ids.get(&instr.id) {
                // The RHS value temp is no longer referenced after the rewrite,
                // so remove it from used_ids to allow pruning the Primitive.
                used_ids.remove(rhs_id);
            }
        }
    }

    // Prune unused instructions
    for (_, block) in &mut func.body.blocks {
        let num_instrs = block.instructions.len();
        let block_kind = block.kind;
        let mut i = 0;
        block.instructions.retain(|instr| {
            let idx = i;
            i += 1;
            let is_block_value = block_kind != BlockKind::Block && idx == num_instrs - 1;
            if is_block_value {
                return true; // Always keep block values
            }
            let preserve_unused_named_declare_local = options
                .preserve_unused_named_declare_locals
                && !rewritten_to_declare_instr_ids.contains(&instr.id)
                && matches!(
                    &instr.value,
                    InstructionValue::DeclareLocal { lvalue, .. }
                        if lvalue.kind == InstructionKind::Let
                            && matches!(lvalue.place.identifier.name, Some(IdentifierName::Named(_)))
                );
            let keep = preserve_unused_named_declare_local
                || is_id_or_name_used(&instr.lvalue.identifier, &used_ids, &used_names)
                || !is_pruneable(
                    &instr.value,
                    &used_ids,
                    &used_names,
                    &load_local_sources,
                    &local_decl_ids,
                    &protected_reassign_place_ids,
                    &captured_context_decl_ids,
                );
            if !keep
                && debug_dce_reassign
                && let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                && lvalue.kind == InstructionKind::Reassign
            {
                let name = match &lvalue.place.identifier.name {
                    Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                        n.as_str()
                    }
                    None => "<unnamed>",
                };
                eprintln!(
                    "[DCE_REASSIGN] pruned bb{} instr#{} name={} id={} decl={} local_decl={} used_id={} used_name={}",
                    block.id.0,
                    instr.id.0,
                    name,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    local_decl_ids.contains(&lvalue.place.identifier.declaration_id),
                    used_ids.contains(&lvalue.place.identifier.id),
                    get_identifier_name(&lvalue.place.identifier)
                        .is_some_and(|n| used_names.contains(&n))
                );
            }
            keep
        });
    }

    // Prune unused elements from Destructure patterns
    prune_destructure_patterns(func, &used_ids, &used_names);

    // Upstream retains only context vars still referenced after DCE rewrites.
    // Without this, stale context entries can block later outlining parity.
    func.context
        .retain(|context_var| is_id_or_name_used(&context_var.identifier, &used_ids, &used_names));

    PRESERVED_TOP_LEVEL_LET_INITIALIZERS.with(|slot| {
        slot.borrow_mut().extend(preserved_initializers);
    });
}

/// Check if a place's identifier is used.
fn is_place_used(
    place: &Place,
    used_ids: &HashSet<IdentifierId>,
    used_names: &HashSet<String>,
) -> bool {
    is_id_or_name_used(&place.identifier, used_ids, used_names)
}

/// Prune unused elements from Destructure patterns.
fn prune_destructure_patterns(
    func: &mut HIRFunction,
    used_ids: &HashSet<IdentifierId>,
    used_names: &HashSet<String>,
) {
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::Destructure { lvalue, .. } = &mut instr.value {
                match &mut lvalue.pattern {
                    Pattern::Array(arr) => {
                        // Replace unused Place elements with Hole
                        for item in arr.items.iter_mut() {
                            match item {
                                ArrayElement::Place(place) => {
                                    if !is_place_used(place, used_ids, used_names) {
                                        *item = ArrayElement::Hole;
                                    }
                                }
                                ArrayElement::Spread(place) => {
                                    if !is_place_used(place, used_ids, used_names) {
                                        *item = ArrayElement::Hole;
                                    }
                                }
                                ArrayElement::Hole => {}
                            }
                        }
                        // Remove trailing holes
                        while arr
                            .items
                            .last()
                            .is_some_and(|e| matches!(e, ArrayElement::Hole))
                        {
                            arr.items.pop();
                        }
                    }
                    Pattern::Object(obj) => {
                        // Match upstream: if a used rest element exists, keep the
                        // full object pattern so rest semantics do not change.
                        let mut next_properties: Option<Vec<ObjectPropertyOrSpread>> =
                            Some(Vec::new());
                        for prop in &obj.properties {
                            match prop {
                                ObjectPropertyOrSpread::Property(p) => {
                                    if is_place_used(&p.place, used_ids, used_names)
                                        && let Some(props) = &mut next_properties
                                    {
                                        props.push(prop.clone());
                                    }
                                }
                                ObjectPropertyOrSpread::Spread(place) => {
                                    if is_place_used(place, used_ids, used_names) {
                                        next_properties = None;
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(next_properties) = next_properties {
                            obj.properties = next_properties;
                        }
                    }
                }
            }
        }
    }
}

fn collect_terminal_used(terminal: &Terminal, used_ids: &mut HashSet<IdentifierId>) {
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
            handler_binding: Some(binding),
            ..
        } => {
            used_ids.insert(binding.identifier.id);
        }
        _ => {}
    }
}

/// Extract all Place references from a destructuring pattern.
fn pattern_places(pattern: &Pattern) -> Vec<&Place> {
    let mut places = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => places.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => places.push(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => places.push(p),
                }
            }
        }
    }
    places
}

/// Get the name of an identifier, if it has one.
fn get_identifier_name(id: &Identifier) -> Option<String> {
    match &id.name {
        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => Some(n.clone()),
        None => None,
    }
}
