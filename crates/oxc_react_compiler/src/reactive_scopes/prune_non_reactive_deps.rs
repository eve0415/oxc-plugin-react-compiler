//! Port of PruneNonReactiveDependencies.ts from upstream React Compiler.
//!
//! PropagateScopeDependencies infers dependencies without considering whether
//! they are actually reactive. This pass prunes dependencies that are
//! guaranteed to be non-reactive (globals, constants, stable refs).
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

/// Prune non-reactive dependencies from all reactive scopes.
///
/// After `infer_reactive_places` has marked `Place::reactive = true` on reactive
/// places, and `propagate_scope_dependencies` has computed scope dependencies,
/// this pass removes any dependency whose base identifier is not reactive.
///
/// When a scope's dependencies are all pruned (all non-reactive), the scope
/// becomes sentinel-based (unconditional on first render, cached forever after).
pub fn prune_non_reactive_dependencies(func: &mut HIRFunction) {
    // Step 1: Collect reactive identifier IDs from `place.reactive` flags
    // and from data-flow propagation through LoadLocal/StoreLocal/PropertyLoad/etc.
    let reactive_ids = collect_reactive_identifiers(func);

    // Step 2: For each scope, filter dependencies to only reactive ones.
    // We need to:
    //   a) Collect unique scopes and their dependencies
    //   b) Filter out non-reactive deps
    //   c) Update the scope annotation on every instruction that references the scope

    // Collect scope info: scope_id → (reactive_deps, all declarations, all reassignments)
    let mut scope_deps: HashMap<ScopeId, Vec<ReactiveScopeDependency>> = HashMap::new();
    let mut scope_seen: HashSet<ScopeId> = HashSet::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope
                && scope_seen.insert(scope.id)
            {
                // First time seeing this scope — filter its dependencies
                let filtered: Vec<ReactiveScopeDependency> = scope
                    .dependencies
                    .iter()
                    .filter(|dep| reactive_ids.contains(&dep.identifier.id))
                    .cloned()
                    .collect();
                scope_deps.insert(scope.id, filtered);
            }
        }
    }

    if scope_deps.is_empty() {
        return;
    }

    // Step 3: If a scope still has reactive deps, mark all its declarations
    // and reassignments as reactive too (they may produce reactive outputs).
    let mut newly_reactive: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope
                && let Some(filtered_deps) = scope_deps.get(&scope.id)
                && !filtered_deps.is_empty()
            {
                // Scope has reactive deps → its declarations are reactive
                for id in scope.declarations.keys() {
                    newly_reactive.insert(*id);
                }
                for reassignment in &scope.reassignments {
                    newly_reactive.insert(reassignment.id);
                }
            }
        }
    }

    // Step 4: Update scope annotations with filtered dependencies
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if let Some(scope) = &mut instr.lvalue.identifier.scope
                && let Some(filtered) = scope_deps.get(&scope.id)
            {
                scope.dependencies = filtered.clone();
            }
        }
    }
}

/// Collect all reactive identifier IDs.
///
/// This is our HIR-based equivalent of upstream's `collectReactiveIdentifiers`.
/// It starts with identifiers already marked `reactive = true` by `infer_reactive_places`,
/// then propagates through data-flow edges (LoadLocal, StoreLocal, PropertyLoad, etc.)
/// matching the upstream Visitor logic in PruneNonReactiveDependencies.ts.
fn collect_reactive_identifiers(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut reactive: HashSet<IdentifierId> = HashSet::new();

    // Seed from place.reactive flags (set by infer_reactive_places)
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                if p.reactive {
                    reactive.insert(p.identifier.id);
                }
            }
        }
    }

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            if phi.place.reactive {
                reactive.insert(phi.place.identifier.id);
            }
            for op in phi.operands.values() {
                if op.reactive {
                    reactive.insert(op.identifier.id);
                }
            }
        }
        for instr in &block.instructions {
            if instr.lvalue.reactive {
                reactive.insert(instr.lvalue.identifier.id);
            }
            // Check all operands for reactive flag
            crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                if place.reactive {
                    reactive.insert(place.identifier.id);
                }
            });
        }
    }

    // Forward propagation: propagate reactivity through data-flow instructions
    // This matches the Visitor in PruneNonReactiveDependencies.ts which extends
    // the reactive set through LoadLocal, StoreLocal, PropertyLoad, ComputedLoad, Destructure.
    loop {
        let mut changed = false;

        for (_, block) in &func.body.blocks {
            // Phi propagation
            for phi in &block.phis {
                if !reactive.contains(&phi.place.identifier.id) {
                    let any_reactive = phi
                        .operands
                        .values()
                        .any(|op| reactive.contains(&op.identifier.id));
                    if any_reactive {
                        reactive.insert(phi.place.identifier.id);
                        changed = true;
                    }
                }
            }

            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        // If source is reactive, lvalue is reactive
                        if reactive.contains(&place.identifier.id)
                            && reactive.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        // If value is reactive, lvalue target and instruction lvalue are reactive
                        if reactive.contains(&value.identifier.id) {
                            if reactive.insert(lvalue.place.identifier.id) {
                                changed = true;
                            }
                            if reactive.insert(instr.lvalue.identifier.id) {
                                changed = true;
                            }
                        }
                    }
                    InstructionValue::PropertyLoad { object, .. } => {
                        // If object is reactive and result is not a stable type, result is reactive
                        // (matches upstream isStableType check in PruneNonReactiveDependencies.ts)
                        if reactive.contains(&object.identifier.id)
                            && !is_stable_type(&instr.lvalue.identifier)
                            && reactive.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::ComputedLoad {
                        object, property, ..
                    } => {
                        // If object OR property is reactive, result is reactive
                        if (reactive.contains(&object.identifier.id)
                            || reactive.contains(&property.identifier.id))
                            && reactive.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::Destructure { lvalue, value, .. } => {
                        // If destructured value is reactive, pattern places (unless stable)
                        // and instruction lvalue are reactive
                        // (matches upstream Destructure case in PruneNonReactiveDependencies.ts)
                        if reactive.contains(&value.identifier.id) {
                            for_each_pattern_place(&lvalue.pattern, &mut |place| {
                                if !is_stable_type(&place.identifier)
                                    && reactive.insert(place.identifier.id)
                                {
                                    changed = true;
                                }
                            });
                            if reactive.insert(instr.lvalue.identifier.id) {
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !changed {
            break;
        }
    }

    reactive
}

/// Check if an identifier is a stable type (setState, dispatch, useRef, startTransition).
/// Matches upstream `isStableType` from HIR.ts.
fn is_stable_type(id: &Identifier) -> bool {
    let shape = match &id.type_ {
        Type::Object {
            shape_id: Some(shape),
        } => Some(shape.as_str()),
        Type::Function {
            shape_id: Some(shape),
            ..
        } => Some(shape.as_str()),
        _ => None,
    };
    matches!(
        shape,
        Some(
            "BuiltInSetState"
                | "BuiltInSetActionState"
                | "BuiltInDispatch"
                | "BuiltInUseRefId"
                | "BuiltInStartTransition"
        )
    )
}

/// Iterate over all Place references in a destructuring pattern.
fn for_each_pattern_place<F: FnMut(&Place)>(pattern: &Pattern, f: &mut F) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => f(place),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => f(&p.place),
                    ObjectPropertyOrSpread::Spread(place) => f(place),
                }
            }
        }
    }
}
