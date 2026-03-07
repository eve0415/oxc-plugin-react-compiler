//! Port of AlignReactiveScopesToBlockScopesHIR.ts.
//!
//! This is the second of four passes that determine how to break a function
//! into discrete reactive scopes (independently memoizable units of code):
//!
//! 1. InferReactiveScopeVariables: assigns identifiers a reactive scope.
//! 2. **AlignReactiveScopesToBlockScopes** (this pass): aligns reactive scope
//!    ranges to block-scope boundaries.
//! 3. MergeOverlappingReactiveScopes: merges any scopes that now overlap.
//! 4. BuildReactiveBlocks: groups statements into ReactiveScopeBlocks.
//!
//! Prior passes assign reactive scopes based on individual instructions at
//! arbitrary control-flow points, but codegen needs scope boundaries aligned
//! to block scopes. For example, if a scope ends partway through an `if`
//! consequent, this pass extends it to the end of that block.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::object_shape::BUILT_IN_JSX_ID;
use crate::hir::types::*;
use crate::hir::visitors;

/// A range representing a block-fallthrough boundary.
#[derive(Debug, Clone)]
struct BlockFallthroughRange {
    range: MutableRange,
    fallthrough: BlockId,
}

/// Tracks a value block context for extending scopes inside value blocks
/// (ternary, logical, optional) to the outer block-scope boundary.
#[derive(Debug, Clone)]
struct ValueBlockNode {
    /// The instruction ID that started this value block.
    _id: InstructionId,
    /// The outer block-scope range that scopes inside the value block
    /// should be extended to.
    value_range: MutableRange,
}

/// Returns the reactive scope of a place if the scope is active at the given
/// instruction id. Mirrors upstream `getPlaceScope(id, place)`.
fn get_place_scope(
    id: InstructionId,
    place: &Place,
    scope_ranges: &HashMap<ScopeId, MutableRange>,
) -> Option<ScopeId> {
    let scope = place.identifier.scope.as_ref()?;
    // Upstream mutates shared scope objects in-place during traversal.
    // Rust identifiers are cloned, so we must consult canonical updated
    // ranges when available to preserve transitive range growth semantics.
    let range = scope_ranges.get(&scope.id).unwrap_or(&scope.range);
    if id >= range.start && id < range.end {
        Some(scope.id)
    } else {
        None
    }
}

/// Align reactive scope ranges to block-scope boundaries.
///
/// Walks the HIR blocks in order and extends scope ranges so that scopes
/// start and end at block boundaries rather than at arbitrary instruction
/// points. This is required because codegen wraps scopes around blocks of
/// code, and a scope cannot start or end in the middle of a block.
pub fn align_reactive_scopes_to_block_scopes(func: &mut HIRFunction) {
    // Phase 1: Compute the updated scope ranges.
    //
    // We cannot mutate scopes in-place during the walk because our scopes
    // are cloned on each Identifier. Instead we collect the *canonical*
    // ranges keyed by ScopeId and propagate them back in Phase 2.
    let mut updated_ranges = compute_aligned_ranges(func);
    extend_scopes_for_flattened_value_expressions(func, &mut updated_ranges);
    extend_scopes_for_flattened_test_roots(func, &mut updated_ranges);

    if updated_ranges.is_empty() {
        return;
    }

    // Phase 2: Propagate the updated ranges to every Identifier that
    // carries the same ScopeId.
    propagate_scope_ranges(func, &updated_ranges);
}

/// Our HIR can represent conditional/logical expressions as flat instructions
/// (`InstructionValue::Ternary` / `InstructionValue::LogicalExpression`) rather
/// than terminal-based value blocks. Upstream aligns scopes through value-block
/// terminals; replicate that behavior here by extending scopes reachable from
/// these flattened value roots to the instruction boundary immediately after the
/// root expression.
fn extend_scopes_for_flattened_value_expressions(
    func: &HIRFunction,
    scope_ranges: &mut HashMap<ScopeId, MutableRange>,
) {
    for (_block_id, block) in &func.body.blocks {
        if block.instructions.is_empty() {
            continue;
        }

        let mut decl_to_instr_idx: HashMap<DeclarationId, usize> = HashMap::new();
        for (idx, instr) in block.instructions.iter().enumerate() {
            decl_to_instr_idx.insert(instr.lvalue.identifier.declaration_id, idx);
            for_each_instruction_defined_place(instr, |place| {
                decl_to_instr_idx.insert(place.identifier.declaration_id, idx);
            });
        }

        let terminal_id = block.terminal.id();

        for (idx, instr) in block.instructions.iter().enumerate() {
            let root_places: Vec<&Place> = match &instr.value {
                InstructionValue::Ternary {
                    test,
                    consequent,
                    alternate,
                    ..
                } => vec![test, consequent, alternate],
                InstructionValue::LogicalExpression { left, right, .. } => vec![left, right],
                _ => continue,
            };
            let should_align_for_jsx = is_jsx_like_type(&instr.lvalue.identifier.type_)
                || root_places
                    .iter()
                    .any(|place| is_jsx_like_type(&place.identifier.type_));
            if !should_align_for_jsx {
                continue;
            }
            let boundary_end = block
                .instructions
                .get(idx + 1)
                .map(|next| next.id)
                .unwrap_or(terminal_id);
            let boundary_is_terminal = boundary_end == terminal_id;

            let mut worklist: Vec<DeclarationId> = Vec::new();
            let mut seen_decls: HashSet<DeclarationId> = HashSet::new();
            let mut scopes_to_extend: HashSet<ScopeId> = HashSet::new();
            let mut saw_nested_value_root = false;

            for place in root_places {
                if let Some(scope_id) = scope_id_at_place(instr.id, place, scope_ranges) {
                    scopes_to_extend.insert(scope_id);
                }
                worklist.push(place.identifier.declaration_id);
            }

            while let Some(decl_id) = worklist.pop() {
                if !seen_decls.insert(decl_id) {
                    continue;
                }
                let Some(&def_idx) = decl_to_instr_idx.get(&decl_id) else {
                    continue;
                };
                if def_idx > idx {
                    continue;
                }
                let def_instr = &block.instructions[def_idx];
                if def_idx != idx
                    && matches!(
                        def_instr.value,
                        InstructionValue::Ternary { .. }
                            | InstructionValue::LogicalExpression { .. }
                    )
                {
                    saw_nested_value_root = true;
                }
                if let Some(scope_id) =
                    scope_id_at_place(def_instr.id, &def_instr.lvalue, scope_ranges)
                {
                    scopes_to_extend.insert(scope_id);
                }
                for_each_instruction_defined_place(def_instr, |place| {
                    if let Some(scope_id) = scope_id_at_place(def_instr.id, place, scope_ranges) {
                        scopes_to_extend.insert(scope_id);
                    }
                });

                visitors::for_each_instruction_operand(def_instr, |operand| {
                    if let Some(scope_id) = scope_id_at_place(def_instr.id, operand, scope_ranges) {
                        scopes_to_extend.insert(scope_id);
                    }
                    worklist.push(operand.identifier.declaration_id);
                });
            }

            if !saw_nested_value_root && !boundary_is_terminal {
                continue;
            }
            for scope_id in scopes_to_extend {
                if let Some(range) = scope_ranges.get_mut(&scope_id)
                    && boundary_end > range.end
                {
                    range.end = boundary_end;
                }
            }
        }
    }
}

/// Additional flattened-lowering alignment for root test expressions.
///
/// When `Ternary` / `LogicalExpression` roots are instruction-level values, upstream
/// value-block alignment keeps the root test chain in-scope through the root
/// boundary. Restrict this to roots whose lvalue has no scope to avoid broad
/// over-merging.
fn extend_scopes_for_flattened_test_roots(
    func: &HIRFunction,
    scope_ranges: &mut HashMap<ScopeId, MutableRange>,
) {
    for (_block_id, block) in &func.body.blocks {
        if block.instructions.is_empty() {
            continue;
        }

        let mut decl_to_instr_idx: HashMap<DeclarationId, usize> = HashMap::new();
        for (idx, instr) in block.instructions.iter().enumerate() {
            decl_to_instr_idx.insert(instr.lvalue.identifier.declaration_id, idx);
            for_each_instruction_defined_place(instr, |place| {
                decl_to_instr_idx.insert(place.identifier.declaration_id, idx);
            });
        }

        let terminal_id = block.terminal.id();

        for (idx, instr) in block.instructions.iter().enumerate() {
            let (test_place, is_ternary, is_logical) = match &instr.value {
                InstructionValue::Ternary { test, .. } => (test, true, false),
                InstructionValue::LogicalExpression { left, .. } => (left, false, true),
                _ => continue,
            };
            if !is_ternary && !is_logical {
                continue;
            }

            if scope_id_at_place(instr.id, &instr.lvalue, scope_ranges).is_some() {
                continue;
            }

            let boundary_end = block
                .instructions
                .get(idx + 1)
                .map(|next| next.id)
                .unwrap_or(terminal_id);

            let mut worklist = vec![test_place.identifier.declaration_id];
            let mut seen_decls: HashSet<DeclarationId> = HashSet::new();
            let mut scopes_to_extend: HashSet<ScopeId> = HashSet::new();
            let mut has_call_like = false;
            let mut has_optional_method_call = false;

            while let Some(decl_id) = worklist.pop() {
                if !seen_decls.insert(decl_id) {
                    continue;
                }
                let Some(&def_idx) = decl_to_instr_idx.get(&decl_id) else {
                    continue;
                };
                if def_idx > idx {
                    continue;
                }
                let def_instr = &block.instructions[def_idx];
                match &def_instr.value {
                    InstructionValue::CallExpression { .. }
                    | InstructionValue::TaggedTemplateExpression { .. } => {
                        has_call_like = true;
                    }
                    InstructionValue::MethodCall {
                        receiver_optional,
                        call_optional,
                        ..
                    } => {
                        has_call_like = true;
                        if *receiver_optional || *call_optional {
                            has_optional_method_call = true;
                        }
                    }
                    _ => {}
                }
                if let Some(scope_id) =
                    scope_id_at_place(def_instr.id, &def_instr.lvalue, scope_ranges)
                {
                    scopes_to_extend.insert(scope_id);
                }
                for_each_instruction_defined_place(def_instr, |place| {
                    if let Some(scope_id) = scope_id_at_place(def_instr.id, place, scope_ranges) {
                        scopes_to_extend.insert(scope_id);
                    }
                });
                visitors::for_each_instruction_operand(def_instr, |operand| {
                    if let Some(scope_id) = scope_id_at_place(def_instr.id, operand, scope_ranges) {
                        scopes_to_extend.insert(scope_id);
                    }
                    worklist.push(operand.identifier.declaration_id);
                });
            }

            let should_apply =
                (is_ternary && has_call_like) || (is_logical && has_optional_method_call);
            if !should_apply {
                continue;
            }
            for scope_id in scopes_to_extend {
                if let Some(range) = scope_ranges.get_mut(&scope_id)
                    && boundary_end > range.end
                {
                    range.end = boundary_end;
                }
            }
        }
    }
}

fn is_jsx_like_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Object {
            shape_id: Some(shape_id),
        } if shape_id == BUILT_IN_JSX_ID
    )
}

fn scope_id_at_place(
    id: InstructionId,
    place: &Place,
    scope_ranges: &mut HashMap<ScopeId, MutableRange>,
) -> Option<ScopeId> {
    let Some(scope_id) = get_place_scope(id, place, scope_ranges) else {
        return None;
    };
    if !scope_ranges.contains_key(&scope_id)
        && let Some(initial_scope) = &place.identifier.scope
    {
        scope_ranges.insert(scope_id, initial_scope.range.clone());
    }
    Some(scope_id)
}

fn for_each_instruction_defined_place<'a>(instr: &'a Instruction, mut f: impl FnMut(&'a Place)) {
    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            f(&lvalue.place);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            f(lvalue);
        }
        _ => {}
    }
}

/// Walk the HIR and compute aligned scope ranges. Returns a map from
/// ScopeId to the updated MutableRange.
fn compute_aligned_ranges(func: &HIRFunction) -> HashMap<ScopeId, MutableRange> {
    // Build a block lookup for O(1) access.
    let block_map: HashMap<BlockId, &BasicBlock> = func
        .body
        .blocks
        .iter()
        .map(|(id, block)| (*id, block))
        .collect();

    // Canonical scope ranges, keyed by ScopeId. We read initial ranges from
    // the first encounter of each scope and then extend them.
    let mut scope_ranges: HashMap<ScopeId, MutableRange> = HashMap::new();

    // Active scopes whose range.end is still beyond the current instruction.
    let mut active_scopes: HashSet<ScopeId> = HashSet::new();

    // Scopes we have already seen (used to guard the value-range extension
    // that only happens on first encounter).
    let mut seen_scopes: HashSet<ScopeId> = HashSet::new();

    // Stack of block-fallthrough ranges. When a terminal with a fallthrough
    // is encountered, we push its range here so that when we reach the
    // fallthrough block we can extend any active scopes.
    let mut active_block_fallthrough_ranges: Vec<BlockFallthroughRange> = Vec::new();

    // Mapping from BlockId -> ValueBlockNode for value blocks.
    let mut value_block_nodes: HashMap<BlockId, ValueBlockNode> = HashMap::new();

    // Helper closure: records a place's scope as active and, on first
    // encounter inside a value block, extends the scope to the value range.
    // We inline this as a local fn that takes the shared state by reference.

    // -- Walk blocks in order --
    for (_block_id, block) in &func.body.blocks {
        let starting_id = block
            .instructions
            .first()
            .map(|i| i.id)
            .unwrap_or_else(|| block.terminal.id());

        // Prune scopes that have ended before this block.
        active_scopes.retain(|scope_id| {
            scope_ranges
                .get(scope_id)
                .is_some_and(|r| r.end > starting_id)
        });

        // Check if we've reached the fallthrough of the topmost block range.
        if let Some(top) = active_block_fallthrough_ranges.last()
            && top.fallthrough == block.id
        {
            let top = active_block_fallthrough_ranges.pop().unwrap();
            // All active scopes overlap this block-fallthrough range,
            // so extend their start to include the range start.
            for scope_id in &active_scopes {
                if let Some(range) = scope_ranges.get_mut(scope_id) {
                    range.start = InstructionId(range.start.0.min(top.range.start.0));
                }
            }
        }

        let node = value_block_nodes.get(&block.id).cloned();

        // Visit instruction lvalues and operands.
        for instr in &block.instructions {
            // Record lvalue.
            record_place(
                instr.id,
                &instr.lvalue,
                &node,
                &mut scope_ranges,
                &mut active_scopes,
                &mut seen_scopes,
            );

            // Record lvalues from store/declare/destructure/update instructions.
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    record_place(
                        instr.id,
                        &lvalue.place,
                        &node,
                        &mut scope_ranges,
                        &mut active_scopes,
                        &mut seen_scopes,
                    );
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    for_each_pattern_place_ref(&lvalue.pattern, |place| {
                        record_place(
                            instr.id,
                            place,
                            &node,
                            &mut scope_ranges,
                            &mut active_scopes,
                            &mut seen_scopes,
                        );
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    record_place(
                        instr.id,
                        lvalue,
                        &node,
                        &mut scope_ranges,
                        &mut active_scopes,
                        &mut seen_scopes,
                    );
                }
                _ => {}
            }

            // Record value operands.
            visitors::for_each_instruction_operand(instr, |operand| {
                record_place(
                    instr.id,
                    operand,
                    &node,
                    &mut scope_ranges,
                    &mut active_scopes,
                    &mut seen_scopes,
                );
            });
        }

        // Record terminal operands.
        let terminal = &block.terminal;
        visitors::for_each_terminal_operand(terminal, |operand| {
            record_place(
                terminal.id(),
                operand,
                &node,
                &mut scope_ranges,
                &mut active_scopes,
                &mut seen_scopes,
            );
        });

        let terminal_id = terminal.id();
        let fallthrough = terminal.fallthrough();
        let is_branch = matches!(terminal, Terminal::Branch { .. });

        if let Some(ft) = fallthrough {
            if !is_branch {
                // Extend active scopes to include the first instruction of the
                // fallthrough block.
                if let Some(ft_block) = block_map.get(&ft) {
                    let next_id = ft_block
                        .instructions
                        .first()
                        .map(|i| i.id)
                        .unwrap_or_else(|| ft_block.terminal.id());

                    for scope_id in &active_scopes {
                        if let Some(range) = scope_ranges.get_mut(scope_id)
                            && range.end > terminal_id
                        {
                            range.end = InstructionId(range.end.0.max(next_id.0));
                        }
                    }

                    // Record the block-fallthrough range for future scopes.
                    active_block_fallthrough_ranges.push(BlockFallthroughRange {
                        fallthrough: ft,
                        range: MutableRange {
                            start: terminal_id,
                            end: next_id,
                        },
                    });
                }

                // Propagate value block node to fallthrough.
                if let Some(ref n) = node {
                    value_block_nodes.entry(ft).or_insert_with(|| n.clone());
                }
            }
        } else if let Terminal::Goto {
            block: goto_target, ..
        } = terminal
        {
            // Goto to a label (not the natural fallthrough). Extend active
            // scopes to include the labeled range.
            let start_entry = active_block_fallthrough_ranges
                .iter()
                .enumerate()
                .find(|(_, r)| r.fallthrough == *goto_target);

            if let Some((idx, _)) = start_entry {
                // Only do this if it's NOT the topmost fallthrough (which
                // would be the natural fallthrough).
                let is_topmost = idx == active_block_fallthrough_ranges.len() - 1;
                if !is_topmost {
                    let start = &active_block_fallthrough_ranges[idx];
                    let start_range_start = start.range.start;
                    let ft_block_id = start.fallthrough;

                    if let Some(ft_block) = block_map.get(&ft_block_id) {
                        let first_id = ft_block
                            .instructions
                            .first()
                            .map(|i| i.id)
                            .unwrap_or_else(|| ft_block.terminal.id());

                        for scope_id in &active_scopes {
                            if let Some(range) = scope_ranges.get_mut(scope_id) {
                                // Only extend scopes that are actually still active.
                                if range.end <= terminal_id {
                                    continue;
                                }
                                range.start = InstructionId(range.start.0.min(start_range_start.0));
                                range.end = InstructionId(range.end.0.max(first_id.0));
                            }
                        }
                    }
                }
            }
        }

        // Visit all terminal successors to set up value block nodes.
        let is_ternary = matches!(terminal, Terminal::Ternary { .. });
        let is_logical = matches!(terminal, Terminal::Logical { .. });
        let is_optional = matches!(terminal, Terminal::Optional { .. });

        for successor in terminal_successors(terminal) {
            if value_block_nodes.contains_key(&successor) {
                continue;
            }

            if let Some(successor_block) = block_map.get(&successor) {
                if successor_block.kind == BlockKind::Block
                    || successor_block.kind == BlockKind::Catch
                {
                    // Block or catch kind -- don't create a value block node.
                    continue;
                }

                if node.is_none() || is_ternary || is_logical || is_optional {
                    // Create a new value block node: transition from non-value
                    // to value block, or from ternary/logical/optional.
                    let value_range = if node.is_none() {
                        // Transition from block -> value block: derive outer range.
                        if let Some(ft) = fallthrough {
                            if let Some(ft_block) = block_map.get(&ft) {
                                let next_id = ft_block
                                    .instructions
                                    .first()
                                    .map(|i| i.id)
                                    .unwrap_or_else(|| ft_block.terminal.id());
                                MutableRange {
                                    start: terminal_id,
                                    end: next_id,
                                }
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    } else {
                        // Value -> value transition: reuse the range.
                        node.as_ref().unwrap().value_range.clone()
                    };

                    value_block_nodes.insert(
                        successor,
                        ValueBlockNode {
                            _id: terminal_id,
                            value_range,
                        },
                    );
                } else {
                    // Value -> value block transition, reuse existing node.
                    if let Some(ref n) = node {
                        value_block_nodes.insert(successor, n.clone());
                    }
                }
            }
        }
    }

    scope_ranges
}

/// Record a place's scope as active. On first encounter inside a value block,
/// extend the scope range to the value-block's outer range.
fn record_place(
    id: InstructionId,
    place: &Place,
    node: &Option<ValueBlockNode>,
    scope_ranges: &mut HashMap<ScopeId, MutableRange>,
    active_scopes: &mut HashSet<ScopeId>,
    seen_scopes: &mut HashSet<ScopeId>,
) {
    // Get the scope ID from the place, checking if it's active.
    let scope_id = match get_place_scope(id, place, scope_ranges) {
        Some(sid) => sid,
        None => return,
    };

    // Ensure we have the canonical range for this scope. If this is the
    // first time we see it, initialize from the identifier's scope.
    if let std::collections::hash_map::Entry::Vacant(e) = scope_ranges.entry(scope_id)
        && let Some(scope) = &place.identifier.scope
    {
        e.insert(scope.range.clone());
    }

    active_scopes.insert(scope_id);

    if seen_scopes.contains(&scope_id) {
        return;
    }
    seen_scopes.insert(scope_id);

    // On first encounter inside a value block, extend scope to the outer range.
    if let Some(vbn) = node
        && let Some(range) = scope_ranges.get_mut(&scope_id)
    {
        range.start = InstructionId(range.start.0.min(vbn.value_range.start.0));
        range.end = InstructionId(range.end.0.max(vbn.value_range.end.0));
    }
}

/// Propagate updated scope ranges to every Identifier in the function.
fn propagate_scope_ranges(func: &mut HIRFunction, ranges: &HashMap<ScopeId, MutableRange>) {
    for (_block_id, block) in &mut func.body.blocks {
        // Phis
        for phi in &mut block.phis {
            update_identifier_scope(&mut phi.place.identifier, ranges);
            for op in phi.operands.values_mut() {
                update_identifier_scope(&mut op.identifier, ranges);
            }
        }

        // Instructions
        for instr in &mut block.instructions {
            // Lvalue
            update_identifier_scope(&mut instr.lvalue.identifier, ranges);

            // Lvalues in instruction value
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    update_identifier_scope(&mut lvalue.place.identifier, ranges);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    map_pattern_identifiers(&mut lvalue.pattern, |ident| {
                        update_identifier_scope(ident, ranges);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    update_identifier_scope(&mut lvalue.identifier, ranges);
                }
                _ => {}
            }

            // Do not propagate outer scope ranges back onto lowered-function
            // captured contexts. Those copies intentionally keep reset ranges
            // so nested function captures don't participate in parent overlap
            // merging.
            if !matches!(
                instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            ) {
                visitors::map_instruction_operands(instr, |place| {
                    update_identifier_scope(&mut place.identifier, ranges);
                });
            }
        }

        // Terminal operands
        visitors::map_terminal_operands(&mut block.terminal, |place| {
            update_identifier_scope(&mut place.identifier, ranges);
        });
    }
}

/// Update a single identifier's scope range if it has a scope in our map.
fn update_identifier_scope(ident: &mut Identifier, ranges: &HashMap<ScopeId, MutableRange>) {
    if let Some(scope) = &mut ident.scope
        && let Some(updated) = ranges.get(&scope.id)
    {
        scope.range.start = updated.start;
        scope.range.end = updated.end;
        // In upstream JS, identifier.mutableRange === scope.range (shared reference),
        // so updating scope.range also updates mutableRange. In Rust, we must do this
        // explicitly.
        ident.mutable_range = updated.clone();
    }
}

/// Iterate over places in a pattern (read-only).
fn for_each_pattern_place_ref(pattern: &Pattern, mut f: impl FnMut(&Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&p.place);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            f(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
    }
}

/// Iterate over identifiers in a pattern (mutable).
fn map_pattern_identifiers(pattern: &mut Pattern, mut f: impl FnMut(&mut Identifier)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(&mut p.identifier),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&mut p.place.identifier);
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            f(&mut place.identifier);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => f(&mut p.identifier),
                }
            }
        }
    }
}

/// Get all successor block IDs from a terminal.
/// This mirrors upstream `mapTerminalSuccessors` which visits ALL successors.
fn terminal_successors(terminal: &Terminal) -> Vec<BlockId> {
    match terminal {
        Terminal::Unsupported { .. } | Terminal::Unreachable { .. } => vec![],
        Terminal::Throw { .. } | Terminal::Return { .. } => vec![],
        Terminal::Goto { block, .. } => vec![*block],
        Terminal::If {
            consequent,
            alternate,
            fallthrough,
            ..
        } => vec![*consequent, *alternate, *fallthrough],
        Terminal::Branch {
            consequent,
            alternate,
            fallthrough,
            ..
        } => vec![*consequent, *alternate, *fallthrough],
        Terminal::Switch {
            cases, fallthrough, ..
        } => {
            let mut succs: Vec<BlockId> = cases.iter().map(|c| c.block).collect();
            succs.push(*fallthrough);
            succs
        }
        Terminal::For {
            init,
            test,
            update,
            loop_block,
            fallthrough,
            ..
        } => {
            let mut succs = vec![*init, *test, *loop_block, *fallthrough];
            if let Some(u) = update {
                succs.push(*u);
            }
            succs
        }
        Terminal::ForOf {
            init,
            test,
            loop_block,
            fallthrough,
            ..
        } => vec![*init, *test, *loop_block, *fallthrough],
        Terminal::ForIn {
            init,
            loop_block,
            fallthrough,
            ..
        } => vec![*init, *loop_block, *fallthrough],
        Terminal::DoWhile {
            loop_block,
            test,
            fallthrough,
            ..
        } => vec![*loop_block, *test, *fallthrough],
        Terminal::While {
            test,
            loop_block,
            fallthrough,
            ..
        } => vec![*test, *loop_block, *fallthrough],
        Terminal::Logical {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Ternary {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Optional {
            test, fallthrough, ..
        } => vec![*test, *fallthrough],
        Terminal::Label {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::Sequence {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::Try {
            block,
            handler,
            fallthrough,
            ..
        } => vec![*block, *handler, *fallthrough],
        Terminal::MaybeThrow {
            continuation,
            handler,
            ..
        } => vec![*continuation, *handler],
        Terminal::Scope {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
        Terminal::PrunedScope {
            block, fallthrough, ..
        } => vec![*block, *fallthrough],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a minimal Place with a scope.
    fn make_place_with_scope(
        ident_id: u32,
        scope_id: u32,
        range_start: u32,
        range_end: u32,
    ) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(ident_id),
                declaration_id: DeclarationId(ident_id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: Some(Box::new(ReactiveScope {
                    id: ScopeId(scope_id),
                    range: MutableRange {
                        start: InstructionId(range_start),
                        end: InstructionId(range_end),
                    },
                    dependencies: vec![],
                    declarations: Default::default(),
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                })),
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    /// Helper to create a minimal Place without a scope.
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

    /// Test: scope that ends in the middle of an if-consequent gets extended
    /// to the fallthrough.
    #[test]
    fn test_scope_extended_to_fallthrough() {
        // Block 0: [instr 1] x = []; terminal(if) -> consequent=1, alternate=2, fallthrough=3
        // Block 1: [instr 2] x.push(a); [instr 3] noop; terminal(goto) -> 3
        // Block 2: [instr 4] noop; terminal(goto) -> 3
        // Block 3: [instr 5] noop; terminal(return)
        //
        // Scope 1 on x covers [1, 3) -- ends at instr 3 which is inside block 1.
        // After alignment, scope should extend to at least instr 5 (start of block 3).

        let mut func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![
                    (
                        BlockId(0),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(0),
                            instructions: vec![Instruction {
                                id: InstructionId(1),
                                lvalue: make_place_with_scope(1, 1, 1, 3),
                                value: InstructionValue::Primitive {
                                    value: PrimitiveValue::Undefined,
                                    loc: SourceLocation::Generated,
                                },
                                loc: SourceLocation::Generated,
                                effects: None,
                            }],
                            terminal: Terminal::If {
                                test: make_place(10),
                                consequent: BlockId(1),
                                alternate: BlockId(2),
                                fallthrough: BlockId(3),
                                id: InstructionId(2),
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
                            instructions: vec![Instruction {
                                id: InstructionId(3),
                                lvalue: make_place(2),
                                value: InstructionValue::Primitive {
                                    value: PrimitiveValue::Undefined,
                                    loc: SourceLocation::Generated,
                                },
                                loc: SourceLocation::Generated,
                                effects: None,
                            }],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: GotoVariant::Break,
                                id: InstructionId(4),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
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
                                    value: PrimitiveValue::Undefined,
                                    loc: SourceLocation::Generated,
                                },
                                loc: SourceLocation::Generated,
                                effects: None,
                            }],
                            terminal: Terminal::Goto {
                                block: BlockId(3),
                                variant: GotoVariant::Break,
                                id: InstructionId(6),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                    (
                        BlockId(3),
                        BasicBlock {
                            kind: BlockKind::Block,
                            id: BlockId(3),
                            instructions: vec![Instruction {
                                id: InstructionId(7),
                                lvalue: make_place(4),
                                value: InstructionValue::Primitive {
                                    value: PrimitiveValue::Undefined,
                                    loc: SourceLocation::Generated,
                                },
                                loc: SourceLocation::Generated,
                                effects: None,
                            }],
                            terminal: Terminal::Return {
                                value: make_place(4),
                                return_variant: ReturnVariant::Explicit,
                                id: InstructionId(8),
                                loc: SourceLocation::Generated,
                            },
                            preds: HashSet::new(),
                            phis: vec![],
                        },
                    ),
                ],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        align_reactive_scopes_to_block_scopes(&mut func);

        // After alignment, scope 1 should have its end extended to at least
        // the start of block 3 (InstructionId 7).
        let scope = func.body.blocks[0].1.instructions[0]
            .lvalue
            .identifier
            .scope
            .as_ref()
            .unwrap();
        assert!(
            scope.range.end.0 >= 7,
            "Expected scope end >= 7 (start of fallthrough block), got {}",
            scope.range.end.0
        );
    }

    /// Test: a function with no scopes should be a no-op.
    #[test]
    fn test_no_scopes_is_noop() {
        let mut func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![(
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
                )],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        // Should not panic.
        align_reactive_scopes_to_block_scopes(&mut func);
    }
}
