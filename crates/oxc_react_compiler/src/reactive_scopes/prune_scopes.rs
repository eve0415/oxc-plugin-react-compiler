//! Scope pruning passes — remove scopes that don't benefit from memoization.
//!
//! Ports of pruneNonEscapingScopes, pruneUnusedScopes, and
//! pruneAlwaysInvalidatingScopes from upstream React Compiler.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;
use crate::hir::visitors;

/// Prune scopes whose ALL outputs are primitives (no JSX, objects, arrays, functions).
///
/// A scope that only produces primitive values doesn't need memoization.
/// This handles the "false_memo" pattern where upstream doesn't memoize
/// because all outputs are primitives.
///
/// Returns the number of scopes that remain after pruning.
pub fn prune_unused_scopes(func: &mut HIRFunction) -> u32 {
    // Step 1: Collect all scope IDs and check what each scope produces.
    let mut all_scope_ids: HashSet<ScopeId> = HashSet::new();
    let mut scopes_with_non_primitive: HashSet<ScopeId> = HashSet::new();
    let mut scopes_with_hooks: HashSet<ScopeId> = HashSet::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                all_scope_ids.insert(scope.id);

                // Check if this instruction produces a non-primitive value
                if produces_non_primitive(&instr.value) {
                    scopes_with_non_primitive.insert(scope.id);
                }

                // Check for hook calls (any call to a function starting with "use")
                match &instr.value {
                    InstructionValue::CallExpression { callee, .. } => {
                        if is_hook_identifier(&callee.identifier) {
                            scopes_with_hooks.insert(scope.id);
                        }
                    }
                    InstructionValue::MethodCall { property, .. } => {
                        if is_hook_identifier(&property.identifier) {
                            scopes_with_hooks.insert(scope.id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Step 1b: Track which identifier IDs flow to named variables.
    // Unlike the old approach (checking StoreLocal within the same scope), this
    // tracks cross-scope data flow: a CallExpression in scope_0 may produce temp_2,
    // which is then stored to a named var `x` in a different (or no) scope.
    let mut flows_to_named: HashSet<IdentifierId> = HashSet::new();

    // Collect all identifier IDs that are directly the value operand of a
    // StoreLocal/StoreContext to a named (or promoted) variable.
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if lvalue.place.identifier.name.is_some() {
                        flows_to_named.insert(value.identifier.id);
                    }
                }
                _ => {}
            }
        }
        // Also: identifiers used as Return terminal values escape the function
        if let Terminal::Return { value, .. } = &block.terminal {
            flows_to_named.insert(value.identifier.id);
        }
    }

    // Backward propagation: if $b flows to named and $b = LoadLocal($a),
    // then $a also flows to named (the value passes through the load).
    // Also propagate through phi nodes: if phi result flows to named,
    // all phi operands also flow to named.
    loop {
        let mut new_entries = Vec::new();
        for (_, block) in &func.body.blocks {
            // Propagate through phi nodes
            for phi in &block.phis {
                if flows_to_named.contains(&phi.place.identifier.id) {
                    for op in phi.operands.values() {
                        if !flows_to_named.contains(&op.identifier.id) {
                            new_entries.push(op.identifier.id);
                        }
                    }
                }
            }
            // Propagate through load instructions
            for instr in &block.instructions {
                if flows_to_named.contains(&instr.lvalue.identifier.id) {
                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            if !flows_to_named.contains(&place.identifier.id) {
                                new_entries.push(place.identifier.id);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        if new_entries.is_empty() {
            break;
        }
        for id in new_entries {
            flows_to_named.insert(id);
        }
    }

    // Determine which scopes have "escaping" outputs
    let mut scopes_with_named_store: HashSet<ScopeId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                // The instruction's output flows to a named variable
                if flows_to_named.contains(&instr.lvalue.identifier.id) {
                    scopes_with_named_store.insert(scope.id);
                }
                // JSX/Object/Array/Function expressions are always "escaping"
                // because they create memoizable values
                match &instr.value {
                    InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. }
                    | InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. }
                    | InstructionValue::FunctionExpression { .. }
                    | InstructionValue::ObjectMethod { .. }
                    | InstructionValue::NewExpression { .. }
                    | InstructionValue::Destructure { .. } => {
                        scopes_with_named_store.insert(scope.id);
                    }
                    _ => {}
                }
            }
        }
    }

    // Step 2: Determine which scopes to prune.
    // A scope is prunable if:
    //   1. It doesn't produce any non-primitive values, OR it produces non-primitives
    //      but none are stored to named variables (e.g., console.log() scopes)
    //   2. Hook-containing scopes are handled by flatten_scopes_with_hooks (separate pass)
    let mut scopes_to_prune: HashSet<ScopeId> = HashSet::new();
    let debug_scopes = std::env::var("DEBUG_SCOPES").is_ok();
    for scope_id in &all_scope_ids {
        if !scopes_with_non_primitive.contains(scope_id) {
            // All primitive outputs — prune
            if debug_scopes {
                eprintln!("[PRUNE_UNUSED] scope_{} pruned: all primitive", scope_id.0);
            }
            scopes_to_prune.insert(*scope_id);
        } else if !scopes_with_named_store.contains(scope_id) {
            // Has non-primitive outputs but none stored to named variables
            // (e.g., console.log() — result is discarded) — prune
            if debug_scopes {
                eprintln!(
                    "[PRUNE_UNUSED] scope_{} pruned: non-primitive but no named store",
                    scope_id.0
                );
            }
            scopes_to_prune.insert(*scope_id);
        } else if debug_scopes {
            eprintln!(
                "[PRUNE_UNUSED] scope_{} KEPT: has non-primitive + named store",
                scope_id.0
            );
        }
    }

    if scopes_to_prune.is_empty() {
        return all_scope_ids.len() as u32;
    }

    // Step 3: Remove scope annotations from all identifiers in pruned scopes.
    for (_, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            prune_identifier_scope(&mut phi.place.identifier, &scopes_to_prune);
            for op in phi.operands.values_mut() {
                prune_identifier_scope(&mut op.identifier, &scopes_to_prune);
            }
        }
        for instr in &mut block.instructions {
            prune_identifier_scope(&mut instr.lvalue.identifier, &scopes_to_prune);
            visitors::map_instruction_operands(instr, |place| {
                prune_identifier_scope(&mut place.identifier, &scopes_to_prune);
            });
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    prune_identifier_scope(&mut lvalue.place.identifier, &scopes_to_prune);
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    prune_identifier_scope(&mut lvalue.identifier, &scopes_to_prune);
                }
                _ => {}
            }
        }
    }

    (all_scope_ids.len() - scopes_to_prune.len()) as u32
}

/// Check if an instruction produces a non-primitive value that would benefit
/// from memoization. Primitives (numbers, strings, booleans, null, undefined)
/// never need memoization because they're immutable.
fn produces_non_primitive(value: &InstructionValue) -> bool {
    match value {
        // These always produce primitives or are side-effect-free ops
        InstructionValue::Primitive { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::PostfixUpdate { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. } => false,

        // Ternary and logical expressions can produce non-primitive values
        // depending on which branch executes (e.g., `a ? [1,2] : b`).
        // Conservatively treat as non-primitive until type inference is ported.
        InstructionValue::Ternary { .. }
        | InstructionValue::LogicalExpression { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::IteratorNext { .. } => true,

        // These produce non-primitive values (objects, arrays, functions, JSX)
        InstructionValue::ObjectExpression { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::Destructure { .. }
        | InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. }
        | InstructionValue::TaggedTemplateExpression { .. } => true,

        // Stores/deletes are side effects — they don't produce new values
        // that would benefit from memoization. PropertyDelete/ComputedDelete
        // always return a boolean (primitive).
        InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedDelete { .. } => false,
    }
}

fn prune_identifier_scope(ident: &mut Identifier, scopes_to_prune: &HashSet<ScopeId>) {
    if let Some(scope) = &ident.scope
        && scopes_to_prune.contains(&scope.id)
    {
        ident.scope = None;
    }
}

fn is_hook_identifier(ident: &Identifier) -> bool {
    match &ident.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            is_hook_name(name)
        }
        _ => false,
    }
}

fn is_hook_name(name: &str) -> bool {
    name.starts_with("use")
        && name.len() > 3
        && name.chars().nth(3).is_some_and(|c| c.is_uppercase())
}

// ---------------------------------------------------------------------------
// pruneAlwaysInvalidatingScopes
// ---------------------------------------------------------------------------

/// Port of PruneAlwaysInvalidatingScopes.ts.
///
/// Some instructions always produce a new value (allocations like arrays, objects,
/// JSX, new expressions). If such a value is NOT memoized (not within any scope),
/// then any scope that depends on it will always invalidate — the dep changes
/// every render. This pass prunes such scopes to avoid wasted comparisons.
///
/// NOTE: function calls are an edge-case. They MAY return primitives, so this
/// pass optimistically assumes they do. Only guaranteed new allocations cause pruning.
pub fn prune_always_invalidating_scopes(func: &mut HIRFunction) {
    let debug = std::env::var("DEBUG_SCOPES").is_ok();

    // Step 1: Collect which identifier IDs are "always invalidating" (fresh allocations)
    // and which are "unmemoized" (always invalidating AND not within a reactive scope).
    let mut always_invalidating: HashSet<IdentifierId> = HashSet::new();
    let mut unmemoized: HashSet<IdentifierId> = HashSet::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let within_scope = instr.lvalue.identifier.scope.is_some();
            let lvalue_id = instr.lvalue.identifier.id;

            match &instr.value {
                // These always produce new allocations
                InstructionValue::ArrayExpression { .. }
                | InstructionValue::ObjectExpression { .. }
                | InstructionValue::JsxExpression { .. }
                | InstructionValue::JsxFragment { .. }
                | InstructionValue::NewExpression { .. } => {
                    always_invalidating.insert(lvalue_id);
                    if !within_scope {
                        unmemoized.insert(lvalue_id);
                    }
                }
                // Propagate through StoreLocal/StoreContext
                InstructionValue::StoreLocal {
                    lvalue: store_lv,
                    value: store_val,
                    ..
                }
                | InstructionValue::StoreContext {
                    lvalue: store_lv,
                    value: store_val,
                    ..
                } => {
                    if always_invalidating.contains(&store_val.identifier.id) {
                        always_invalidating.insert(store_lv.place.identifier.id);
                    }
                    if unmemoized.contains(&store_val.identifier.id) {
                        unmemoized.insert(store_lv.place.identifier.id);
                    }
                }
                // Propagate through LoadLocal/LoadContext
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if always_invalidating.contains(&place.identifier.id) {
                        always_invalidating.insert(lvalue_id);
                    }
                    if unmemoized.contains(&place.identifier.id) {
                        unmemoized.insert(lvalue_id);
                    }
                }
                _ => {}
            }
        }
    }

    if unmemoized.is_empty() {
        return;
    }

    // Step 2: For each scope, collect:
    //   - dependencies (from scope.dependencies)
    //   - declarations (lvalue identifiers within the scope)
    //   - operands (identifiers read by instructions within the scope)
    let mut scope_deps: HashMap<ScopeId, Vec<IdentifierId>> = HashMap::new();
    let mut scope_decl_ids: HashMap<ScopeId, Vec<IdentifierId>> = HashMap::new();
    let mut scope_reads: HashMap<ScopeId, Vec<IdentifierId>> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                let sid = scope.id;
                scope_decl_ids
                    .entry(sid)
                    .or_default()
                    .push(instr.lvalue.identifier.id);

                for dep in &scope.dependencies {
                    scope_deps.entry(sid).or_default().push(dep.identifier.id);
                }

                visitors::for_each_instruction_operand(instr, |place| {
                    scope_reads
                        .entry(sid)
                        .or_default()
                        .push(place.identifier.id);
                });
            }
        }
    }

    // Step 3: Iterative pruning — keep pruning until no more scopes are affected.
    // When a scope is pruned, its always-invalidating declarations become unmemoized,
    // which may cascade to other scopes that read from them.
    //
    // We also need to re-propagate unmemoized through Store/Load chains after each
    // round of pruning, because new unmemoized IDs may flow to identifiers read
    // by other scopes.
    let mut scopes_to_prune: HashSet<ScopeId> = HashSet::new();

    // Collect all Store/Load edges for re-propagation
    let mut store_edges: Vec<(IdentifierId, IdentifierId)> = Vec::new(); // (from_id, to_id)
    let mut load_edges: Vec<(IdentifierId, IdentifierId)> = Vec::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal {
                    lvalue: store_lv,
                    value: store_val,
                    ..
                }
                | InstructionValue::StoreContext {
                    lvalue: store_lv,
                    value: store_val,
                    ..
                } => {
                    store_edges.push((store_val.identifier.id, store_lv.place.identifier.id));
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    load_edges.push((place.identifier.id, instr.lvalue.identifier.id));
                }
                _ => {}
            }
        }
    }

    loop {
        let mut changed = false;

        // Check deps
        for (scope_id, deps) in &scope_deps {
            if scopes_to_prune.contains(scope_id) {
                continue;
            }
            for dep_id in deps {
                if unmemoized.contains(dep_id) {
                    if debug {
                        eprintln!(
                            "[PRUNE_INVALIDATING] scope_{} PRUNED: dep {:?} is unmemoized",
                            scope_id.0, dep_id
                        );
                    }
                    scopes_to_prune.insert(*scope_id);
                    changed = true;
                    break;
                }
            }
        }

        // Check operand reads — but only for reads from OUTSIDE the scope.
        // Upstream only checks scope.dependencies in the ReactiveFunction tree,
        // but we're operating on HIR with scope annotations, so we also check
        // operand reads (excluding scope-internal declarations) as a proxy.
        for (scope_id, reads) in &scope_reads {
            if scopes_to_prune.contains(scope_id) {
                continue;
            }
            let decls = scope_decl_ids.get(scope_id);
            for read_id in reads {
                if unmemoized.contains(read_id) {
                    let is_internal = decls.is_some_and(|d| d.contains(read_id));
                    if !is_internal {
                        if debug {
                            eprintln!(
                                "[PRUNE_INVALIDATING] scope_{} PRUNED: reads unmemoized {:?}",
                                scope_id.0, read_id
                            );
                        }
                        scopes_to_prune.insert(*scope_id);
                        changed = true;
                        break;
                    }
                }
            }
        }

        if !changed {
            break;
        }

        // Propagate: newly pruned scopes' always-invalidating decls become unmemoized
        for scope_id in &scopes_to_prune {
            if let Some(decls) = scope_decl_ids.get(scope_id) {
                for decl_id in decls {
                    if always_invalidating.contains(decl_id) {
                        unmemoized.insert(*decl_id);
                    }
                }
            }
        }

        // Re-propagate unmemoized through Store/Load chains
        loop {
            let mut propagated = false;
            for &(from, to) in &store_edges {
                if unmemoized.contains(&from)
                    && always_invalidating.contains(&to)
                    && !unmemoized.contains(&to)
                {
                    unmemoized.insert(to);
                    propagated = true;
                }
            }
            for &(from, to) in &load_edges {
                if unmemoized.contains(&from)
                    && always_invalidating.contains(&to)
                    && !unmemoized.contains(&to)
                {
                    unmemoized.insert(to);
                    propagated = true;
                }
            }
            if !propagated {
                break;
            }
        }
    }

    if scopes_to_prune.is_empty() {
        return;
    }

    // Step 4: Remove scope annotations from pruned scopes
    remove_scope_annotations(func, &scopes_to_prune);
}

/// Remove scope annotations from all identifiers in the given set of scopes.
fn remove_scope_annotations(func: &mut HIRFunction, scopes_to_prune: &HashSet<ScopeId>) {
    for (_, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            prune_identifier_scope(&mut phi.place.identifier, scopes_to_prune);
            for op in phi.operands.values_mut() {
                prune_identifier_scope(&mut op.identifier, scopes_to_prune);
            }
        }
        for instr in &mut block.instructions {
            prune_identifier_scope(&mut instr.lvalue.identifier, scopes_to_prune);
            visitors::map_instruction_operands(instr, |place| {
                prune_identifier_scope(&mut place.identifier, scopes_to_prune);
            });
            visitors::map_instruction_lvalues(instr, |place| {
                prune_identifier_scope(&mut place.identifier, scopes_to_prune);
            });
        }
        visitors::map_terminal_operands(&mut block.terminal, |place| {
            prune_identifier_scope(&mut place.identifier, scopes_to_prune);
        });
    }
}

// ---------------------------------------------------------------------------
// flattenScopesWithHooksOrUseHIR
// ---------------------------------------------------------------------------

/// Port of FlattenScopesWithHooksOrUseHIR.ts.
///
/// Hooks cannot be called conditionally, so any reactive scope that contains
/// a hook call must be removed entirely. Without this, hook calls would end up
/// inside `if ($[N] !== dep)` guards, violating Rules of Hooks.
pub fn flatten_scopes_with_hooks(func: &mut HIRFunction) {
    let debug = std::env::var("DEBUG_SCOPES").is_ok();

    // Build identifier-name lookup for hook detection.
    let id_to_name = build_name_lookup(func);

    // Step 1: Collect all scope IDs and their ranges.
    let mut scope_ranges: HashMap<ScopeId, MutableRange> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                scope_ranges.entry(scope.id).or_insert(scope.range.clone());
            }
        }
    }

    // Step 2: Find hook call instruction IDs.
    let mut hook_instr_ids: Vec<InstructionId> = Vec::new();
    let mut scopes_with_hooks: HashSet<ScopeId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if is_hook_call(instr, &id_to_name) {
                hook_instr_ids.push(instr.id);
                // Also add the hook's own scope (if any)
                if let Some(scope) = &instr.lvalue.identifier.scope {
                    scopes_with_hooks.insert(scope.id);
                }
            }
        }
    }

    // Step 3: For each hook call, find ALL scopes whose range contains it.
    for hook_id in &hook_instr_ids {
        for (scope_id, range) in &scope_ranges {
            if hook_id.0 >= range.start.0 && hook_id.0 < range.end.0 {
                scopes_with_hooks.insert(*scope_id);
            }
        }
    }

    if scopes_with_hooks.is_empty() {
        return;
    }

    if debug {
        eprintln!(
            "[FLATTEN] Removing scopes with hooks: {:?}",
            scopes_with_hooks.iter().map(|s| s.0).collect::<Vec<_>>()
        );
    }

    // Remove scope annotations from all identifiers in hook-containing scopes.
    remove_scope_annotations(func, &scopes_with_hooks);
}

/// Build identifier-name lookup for hook detection.
fn build_name_lookup(func: &HIRFunction) -> HashMap<IdentifierId, String> {
    let mut id_to_name: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(IdentifierName::Named(name)) = &instr.lvalue.identifier.name {
                id_to_name.insert(instr.lvalue.identifier.id, name.clone());
            }
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(IdentifierName::Named(name)) = &place.identifier.name {
                        id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                    }
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    id_to_name.insert(instr.lvalue.identifier.id, binding.name().to_string());
                }
                InstructionValue::Primitive { value, .. } => {
                    if let PrimitiveValue::String(name) = value {
                        id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                    }
                }
                _ => {}
            }
        }
    }
    id_to_name
}

/// Check if an instruction is a hook call.
fn is_hook_call(instr: &Instruction, id_to_name: &HashMap<IdentifierId, String>) -> bool {
    match &instr.value {
        InstructionValue::CallExpression { callee, .. } => {
            is_hook_identifier(&callee.identifier)
                || id_to_name
                    .get(&callee.identifier.id)
                    .is_some_and(|n| is_hook_name(n))
        }
        InstructionValue::MethodCall { property, .. } => {
            is_hook_identifier(&property.identifier)
                || id_to_name
                    .get(&property.identifier.id)
                    .is_some_and(|n| is_hook_name(n))
        }
        _ => false,
    }
}

/// Count unique reactive scopes surviving in the HIR.
pub fn count_surviving_scopes(func: &HIRFunction) -> usize {
    let mut scope_ids = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                scope_ids.insert(scope.id);
            }
        }
    }
    scope_ids.len()
}
