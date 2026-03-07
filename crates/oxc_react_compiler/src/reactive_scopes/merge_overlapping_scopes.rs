//! Port of MergeOverlappingReactiveScopesHIR.ts.
//!
//! Merges reactive scopes whose instruction ranges overlap. When two scopes
//! overlap, they must be merged into a single scope to ensure correct
//! memoization boundaries. Also merges scopes when an instruction mutates
//! an outer scope while an inner scope is active.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::HashMap;

use crate::hir::types::*;
use crate::hir::visitors;

/// Merge reactive scopes whose instruction ranges overlap.
///
/// Port of upstream `mergeOverlappingReactiveScopesHIR`. The algorithm:
///
/// 1. Collect all scopes and their ranges from place annotations.
/// 2. Build sorted lists of scope starts and scope ends.
/// 3. Walk through instructions in order, maintaining an active scope stack.
/// 4. When a scope ends while scopes that started later are still active,
///    merge them (they overlap).
/// 5. When an instruction mutates an outer scope while an inner scope is on
///    top, merge from the outer scope through the top.
/// 6. Rewrite all scope references to point to the merged group representative.
pub fn merge_overlapping_reactive_scopes(func: &mut HIRFunction) {
    if std::env::var("DISABLE_OVERLAP_MERGE").is_ok() {
        return;
    }

    // Step 1: Collect scope info — all places with scopes and scope ranges.
    // We need to collect scopes eagerly because some scopes begin before
    // the first instruction that references them (due to alignReactiveScopesToBlocks).
    let mut scope_map: HashMap<ScopeId, ReactiveScope> = HashMap::new();
    let mut place_scopes: Vec<(IdentifierId, ScopeId)> = Vec::new();

    let collect_place_scope =
        |ident: &Identifier,
         scope_map: &mut HashMap<ScopeId, ReactiveScope>,
         place_scopes: &mut Vec<(IdentifierId, ScopeId)>| {
            if let Some(scope) = &ident.scope
                && scope.range.start != scope.range.end
            {
                scope_map
                    .entry(scope.id)
                    .or_insert_with(|| (**scope).clone());
                place_scopes.push((ident.id, scope.id));
            }
        };

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            // Collect lvalues (upstream: eachInstructionLValue)
            collect_place_scope(&instr.lvalue.identifier, &mut scope_map, &mut place_scopes);
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    collect_place_scope(
                        &lvalue.place.identifier,
                        &mut scope_map,
                        &mut place_scopes,
                    );
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    collect_place_scope(&lvalue.identifier, &mut scope_map, &mut place_scopes);
                }
                _ => {}
            }
            // Collect operands (upstream: eachInstructionOperand)
            visitors::for_each_instruction_operand(instr, |place| {
                collect_place_scope(&place.identifier, &mut scope_map, &mut place_scopes);
            });
        }
        // Collect terminal operands (upstream: eachTerminalOperand)
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            collect_place_scope(&place.identifier, &mut scope_map, &mut place_scopes);
        });
    }

    if scope_map.len() < 2 {
        return;
    }

    // Step 2: Build sorted scope start/end lists (sorted descending so we can pop from the end).
    let mut scope_starts: Vec<(InstructionId, Vec<ScopeId>)>;
    let mut scope_ends: Vec<(InstructionId, Vec<ScopeId>)>;

    {
        let mut starts_map: HashMap<u32, Vec<ScopeId>> = HashMap::new();
        let mut ends_map: HashMap<u32, Vec<ScopeId>> = HashMap::new();
        for (scope_id, scope) in &scope_map {
            starts_map
                .entry(scope.range.start.0)
                .or_default()
                .push(*scope_id);
            ends_map
                .entry(scope.range.end.0)
                .or_default()
                .push(*scope_id);
        }
        scope_starts = starts_map
            .into_iter()
            .map(|(id, scopes)| (InstructionId::new(id), scopes))
            .collect();
        scope_ends = ends_map
            .into_iter()
            .map(|(id, scopes)| (InstructionId::new(id), scopes))
            .collect();
    }

    // Sort descending by instruction ID so we can pop from the end
    scope_starts.sort_by_key(|b| std::cmp::Reverse(b.0.0));
    scope_ends.sort_by_key(|b| std::cmp::Reverse(b.0.0));

    // Step 3: Walk through instructions, maintaining active scope stack.
    // Union-find for merging scopes.
    let mut uf_parent: HashMap<ScopeId, ScopeId> = HashMap::new();

    let find = |uf: &mut HashMap<ScopeId, ScopeId>, mut id: ScopeId| -> ScopeId {
        let mut root = id;
        while let Some(&parent) = uf.get(&root) {
            if parent == root {
                break;
            }
            root = parent;
        }
        // Path compression
        while id != root {
            if let Some(&parent) = uf.get(&id) {
                uf.insert(id, root);
                id = parent;
            } else {
                break;
            }
        }
        root
    };

    let union = |uf: &mut HashMap<ScopeId, ScopeId>, ids: &[ScopeId]| {
        if ids.len() < 2 {
            return;
        }
        let root = find(uf, ids[0]);
        for &id in &ids[1..] {
            let r = find(uf, id);
            if r != root {
                uf.insert(r, root);
            }
        }
    };

    // Initialize union-find
    for &scope_id in scope_map.keys() {
        uf_parent.entry(scope_id).or_insert(scope_id);
    }

    // Active scope stack — ordered by start descending (most recent on top)
    let mut active_scopes: Vec<ScopeId> = Vec::new();

    // Helper: get the active scope for a place at the current instruction id.
    // Mirrors upstream `getPlaceScope(id, place)`.
    let get_place_scope = |id: InstructionId, ident: &Identifier| -> Option<ScopeId> {
        let scope = ident.scope.as_ref()?;
        if id >= scope.range.start && id < scope.range.end {
            Some(scope.id)
        } else {
            None
        }
    };

    // Helper: check if a place is mutable at an instruction
    let is_mutable_at = |instr_id: InstructionId, ident: &Identifier| -> bool {
        instr_id.0 >= ident.mutable_range.start.0 && instr_id.0 < ident.mutable_range.end.0
    };

    // Port of upstream visitInstructionId: process scope ends/starts at a given ID
    let visit_instruction_id = |id: InstructionId,
                                scope_ends: &mut Vec<(InstructionId, Vec<ScopeId>)>,
                                scope_starts: &mut Vec<(InstructionId, Vec<ScopeId>)>,
                                active_scopes: &mut Vec<ScopeId>,
                                scope_map: &HashMap<ScopeId, ReactiveScope>,
                                uf: &mut HashMap<ScopeId, ScopeId>| {
        // Handle scope ends
        if let Some(last) = scope_ends.last()
            && last.0.0 <= id.0
        {
            let (_, ending_scopes) = scope_ends.pop().unwrap();
            let mut sorted_ending: Vec<ScopeId> = ending_scopes;
            sorted_ending.sort_by(|a, b| {
                let sa = scope_map.get(a).map(|s| s.range.start.0).unwrap_or(0);
                let sb = scope_map.get(b).map(|s| s.range.start.0).unwrap_or(0);
                sb.cmp(&sa)
            });
            for scope_id in sorted_ending {
                if let Some(idx) = active_scopes.iter().position(|s| *s == scope_id) {
                    if idx != active_scopes.len() - 1 {
                        let mut to_merge = vec![scope_id];
                        to_merge.extend_from_slice(&active_scopes[idx + 1..]);
                        union(uf, &to_merge);
                    }
                    active_scopes.remove(idx);
                }
            }
        }

        // Handle scope starts
        if let Some(last) = scope_starts.last()
            && last.0.0 <= id.0
        {
            let (_, starting_scopes) = scope_starts.pop().unwrap();
            let mut sorted_starting: Vec<ScopeId> = starting_scopes;
            sorted_starting.sort_by(|a, b| {
                let ea = scope_map.get(a).map(|s| s.range.end.0).unwrap_or(0);
                let eb = scope_map.get(b).map(|s| s.range.end.0).unwrap_or(0);
                eb.cmp(&ea)
            });
            active_scopes.extend(&sorted_starting);
            for i in 1..sorted_starting.len() {
                let prev = sorted_starting[i - 1];
                let curr = sorted_starting[i];
                let prev_end = scope_map.get(&prev).map(|s| s.range.end.0).unwrap_or(0);
                let curr_end = scope_map.get(&curr).map(|s| s.range.end.0).unwrap_or(0);
                if prev_end == curr_end {
                    union(uf, &[prev, curr]);
                }
            }
        }
    };

    // Port of upstream visitPlace: merge scopes when a place mutates an outer scope
    let visit_place = |id: InstructionId,
                       ident: &Identifier,
                       active_scopes: &[ScopeId],
                       uf: &mut HashMap<ScopeId, ScopeId>| {
        if let Some(place_scope_id) = get_place_scope(id, ident)
            && is_mutable_at(id, ident)
            && let Some(idx) = active_scopes.iter().position(|s| *s == place_scope_id)
            && idx != active_scopes.len() - 1
        {
            let mut to_merge = vec![place_scope_id];
            to_merge.extend_from_slice(&active_scopes[idx + 1..]);
            union(uf, &to_merge);
        }
    };

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let id = instr.id;

            // Process scope ends/starts at this instruction (upstream: visitInstructionId)
            visit_instruction_id(
                id,
                &mut scope_ends,
                &mut scope_starts,
                &mut active_scopes,
                &scope_map,
                &mut uf_parent,
            );

            // Visit operands — if an instruction mutates an outer scope,
            // merge all scopes from that outer scope to the top
            let is_func_expr = matches!(
                &instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            );

            visitors::for_each_instruction_operand(instr, |place| {
                if is_func_expr && matches!(place.identifier.type_, Type::Primitive) {
                    return;
                }
                visit_place(id, &place.identifier, &active_scopes, &mut uf_parent);
            });

            // Visit lvalues (upstream: eachInstructionLValue)
            visit_place(id, &instr.lvalue.identifier, &active_scopes, &mut uf_parent);
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    visit_place(id, &lvalue.place.identifier, &active_scopes, &mut uf_parent);
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    visit_place(id, &lvalue.identifier, &active_scopes, &mut uf_parent);
                }
                _ => {}
            }
        }

        // Process terminal: scope ends/starts at terminal ID (upstream: visitInstructionId)
        let terminal_id = crate::hir::types::get_terminal_id(&block.terminal);
        visit_instruction_id(
            terminal_id,
            &mut scope_ends,
            &mut scope_starts,
            &mut active_scopes,
            &scope_map,
            &mut uf_parent,
        );

        // Visit terminal operands (upstream: eachTerminalOperand)
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            visit_place(
                terminal_id,
                &place.identifier,
                &active_scopes,
                &mut uf_parent,
            );
        });
    }

    // Step 4: Build the merge map and update scope ranges.
    let mut merge_map: HashMap<ScopeId, ScopeId> = HashMap::new();
    let mut any_merged = false;
    for &scope_id in scope_map.keys() {
        let root = find(&mut uf_parent, scope_id);
        if root != scope_id {
            merge_map.insert(scope_id, root);
            any_merged = true;
        }
    }

    if !any_merged {
        return;
    }

    // Merge scope ranges: the merged scope gets the union of all member ranges
    let mut merged_ranges: HashMap<ScopeId, MutableRange> = HashMap::new();
    for (scope_id, scope) in &scope_map {
        let root = find(&mut uf_parent, *scope_id);
        let entry = merged_ranges.entry(root).or_insert(scope.range.clone());
        entry.start = InstructionId::new(entry.start.0.min(scope.range.start.0));
        entry.end = InstructionId::new(entry.end.0.max(scope.range.end.0));
    }

    // Step 5: Rewrite all scope annotations.
    for (_, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            remap_scope(&mut phi.place.identifier, &merge_map, &merged_ranges);
            for op in phi.operands.values_mut() {
                remap_scope(&mut op.identifier, &merge_map, &merged_ranges);
            }
        }
        for instr in &mut block.instructions {
            remap_scope(&mut instr.lvalue.identifier, &merge_map, &merged_ranges);
            visitors::map_instruction_operands(instr, |place| {
                remap_scope(&mut place.identifier, &merge_map, &merged_ranges);
            });
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    remap_scope(&mut lvalue.place.identifier, &merge_map, &merged_ranges);
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    remap_scope(&mut lvalue.identifier, &merge_map, &merged_ranges);
                }
                _ => {}
            }
        }
    }
}

fn remap_scope(
    ident: &mut Identifier,
    merge_map: &HashMap<ScopeId, ScopeId>,
    merged_ranges: &HashMap<ScopeId, MutableRange>,
) {
    if let Some(scope) = &mut ident.scope {
        if let Some(&merged_id) = merge_map.get(&scope.id) {
            scope.id = merged_id;
        }
        if let Some(range) = merged_ranges.get(&scope.id) {
            scope.range = range.clone();
            // In upstream JS, identifier.mutableRange === scope.range (shared reference),
            // so updating scope.range also updates mutableRange. In Rust, we must do this
            // explicitly.
            ident.mutable_range = range.clone();
        }
    }
}
