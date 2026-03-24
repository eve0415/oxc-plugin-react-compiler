//! Infer reactive places — simplified port of InferReactivePlaces.ts.
//!
//! Marks which identifiers are "reactive" (derived from props, hooks, or state).
//! This is used to determine scope boundaries: reactive allocations go inside
//! memo scopes, while non-reactive pure reads go outside.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::dominator::{PostDominator, PostDominatorOptions, compute_post_dominator_tree};
use crate::hir::types::*;
use crate::hir::visitors;
use crate::reactive_scopes::infer_scope_variables::compute_disjoint_mutable_alias_roots;

/// Mark reactive places on a function's HIR in-place.
///
/// A place is reactive if:
/// 1. It's a function parameter (props can change)
/// 2. It's the result of a hook call (hooks access state/context)
/// 3. It's derived from a reactive operand (transitively)
///
/// After this pass, `Place::reactive` is set to `true` for reactive places.
///
/// Returns true if any places are reactive (i.e., the function needs memoization).
pub fn infer_reactive_places(func: &mut HIRFunction) -> bool {
    // Build identifier-name lookup. Call expressions use temporary identifiers
    // that don't carry the original name. Trace through LoadLocal/LoadGlobal
    // to recover original names for hook detection.
    let mut id_to_name: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(IdentifierName::Named(name) | IdentifierName::Promoted(name)) =
                        &place.identifier.name
                    {
                        id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                    } else if let Some(mapped) = id_to_name.get(&place.identifier.id) {
                        id_to_name.insert(instr.lvalue.identifier.id, mapped.clone());
                    }
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    id_to_name.insert(instr.lvalue.identifier.id, binding.name().to_string());
                }
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(name),
                    ..
                } => {
                    id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                }
                InstructionValue::PropertyLoad {
                    property: PropertyLiteral::String(name),
                    ..
                } => {
                    id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                }
                InstructionValue::ComputedLoad { property, .. } => {
                    if let Some(mapped) = id_to_name.get(&property.identifier.id) {
                        id_to_name.insert(instr.lvalue.identifier.id, mapped.clone());
                    }
                }
                _ => {}
            }
        }
    }

    let mut alias_roots = compute_disjoint_mutable_alias_roots(func);
    let stable_ref_ids = collect_builtin_use_ref_ids(func);
    alias_roots.retain(|id, root| !stable_ref_ids.contains(id) && !stable_ref_ids.contains(root));
    let mut reactive_ids: HashSet<IdentifierId> = HashSet::new();
    let block_index = build_block_index(func);
    let post_dominators = compute_post_dominator_tree(
        func,
        PostDominatorOptions {
            include_throws_as_exit_node: false,
        },
    );
    let mut post_dominator_frontier_cache: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();

    // Step 1: Mark parameters as reactive
    for param in &mut func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                let inserted =
                    mark_reactive_identifier_id(&mut reactive_ids, &alias_roots, p.identifier.id);
                debug_reactivity_mark("param", &p.identifier, inserted);
                p.reactive = true;
            }
        }
    }

    // Step 2: Forward propagation with fixpoint iteration
    // We iterate until no new reactive IDs are discovered.
    loop {
        let mut changed = false;

        // Identify blocks that are reactively controlled.
        // This mirrors upstream's post-dominator-frontier based control tracking.
        let mut reactively_controlled_blocks: HashSet<BlockId> = HashSet::new();
        for (block_id, _) in &func.body.blocks {
            if is_reactive_controlled_block(
                *block_id,
                func,
                &post_dominators,
                &block_index,
                &mut post_dominator_frontier_cache,
                &reactive_ids,
                &alias_roots,
            ) {
                reactively_controlled_blocks.insert(*block_id);
            }
        }

        for (bid, block) in &mut func.body.blocks {
            let has_reactive_control = reactively_controlled_blocks.contains(bid);
            // Process phis: if any phi operand is reactive OR comes from a
            // reactively-controlled block, the phi result is reactive.
            for phi in &mut block.phis {
                if is_reactive_identifier_id(&reactive_ids, &alias_roots, phi.place.identifier.id) {
                    // ID is in reactive set (possibly via alias or lvalue propagation)
                    // but ensure the Place's .reactive flag is also set.
                    phi.place.reactive = true;
                    continue;
                }
                let any_reactive = phi.operands.values().any(|op| {
                    is_reactive_identifier_id(&reactive_ids, &alias_roots, op.identifier.id)
                });
                // Control-flow reactivity: if any operand comes from a block
                // that's reached through a reactive branch condition, the phi
                // result depends on reactive control flow.
                let control_flow_reactive = phi
                    .operands
                    .keys()
                    .any(|block_id| reactively_controlled_blocks.contains(block_id));
                if any_reactive || control_flow_reactive {
                    let inserted = mark_reactive_identifier_id(
                        &mut reactive_ids,
                        &alias_roots,
                        phi.place.identifier.id,
                    );
                    debug_reactivity_mark("phi", &phi.place.identifier, inserted);
                    phi.place.reactive = true;
                    changed = true;
                }
            }

            for instr in &mut block.instructions {
                // Check if this is a stable hook call (like useRef) whose result
                // should NOT be reactive regardless of its arguments.
                let is_stable_hook_call = match &instr.value {
                    InstructionValue::CallExpression { callee, .. } => {
                        let is_hook_or_use = is_hook_identifier(callee)
                            || is_use_operator_identifier(callee)
                            || id_to_name
                                .get(&callee.identifier.id)
                                .is_some_and(|n| is_hook_name(n) || is_use_operator_name(n));
                        let is_stable = is_stable_hook(callee)
                            || id_to_name
                                .get(&callee.identifier.id)
                                .is_some_and(|n| n == "useRef");
                        is_hook_or_use && is_stable
                    }
                    InstructionValue::MethodCall { property, .. } => {
                        let is_hook_or_use = is_hook_identifier(property)
                            || is_use_operator_identifier(property)
                            || id_to_name
                                .get(&property.identifier.id)
                                .is_some_and(|n| is_hook_name(n) || is_use_operator_name(n));
                        let is_stable = is_stable_hook(property)
                            || id_to_name
                                .get(&property.identifier.id)
                                .is_some_and(|n| n == "useRef");
                        is_hook_or_use && is_stable
                    }
                    _ => false,
                };

                // Check if any operand is reactive
                let mut has_reactive_input = false;
                // For stable hooks (useRef), don't propagate reactivity from
                // arguments to the result — the ref object is always stable.
                if !is_stable_hook_call {
                    visitors::map_instruction_operands(instr, |place| {
                        if is_reactive_identifier_id(
                            &reactive_ids,
                            &alias_roots,
                            place.identifier.id,
                        ) {
                            place.reactive = true;
                            has_reactive_input = true;
                        }
                    });
                }

                // Hook calls are sources of reactivity.
                // Exception: useRef returns a stable ref object (same across renders).
                let is_hook_call = match &instr.value {
                    InstructionValue::CallExpression { callee, .. } => {
                        let is_hook_or_use = is_hook_identifier(callee)
                            || is_use_operator_identifier(callee)
                            || id_to_name
                                .get(&callee.identifier.id)
                                .is_some_and(|n| is_hook_name(n) || is_use_operator_name(n));
                        let is_stable = is_stable_hook(callee)
                            || id_to_name
                                .get(&callee.identifier.id)
                                .is_some_and(|n| n == "useRef");
                        is_hook_or_use && !is_stable
                    }
                    InstructionValue::MethodCall { property, .. } => {
                        let is_hook_or_use = is_hook_identifier(property)
                            || is_use_operator_identifier(property)
                            || id_to_name
                                .get(&property.identifier.id)
                                .is_some_and(|n| is_hook_name(n) || is_use_operator_name(n));
                        let is_stable = is_stable_hook(property)
                            || id_to_name
                                .get(&property.identifier.id)
                                .is_some_and(|n| n == "useRef");
                        is_hook_or_use && !is_stable
                    }
                    _ => false,
                };
                if is_hook_call {
                    has_reactive_input = true;
                }

                // If any input is reactive, mark instruction lvalues as reactive.
                // Upstream InferReactivePlaces marks each instruction lvalue (including
                // destructure pattern operands), with one exception: stable outputs
                // destructured from stable containers/sources.
                if has_reactive_input {
                    let stable_destructure_source = match &instr.value {
                        InstructionValue::Destructure { value, .. } => {
                            is_stable_type_identifier(&value.identifier)
                                || is_stable_type_container_identifier(&value.identifier)
                        }
                        _ => false,
                    };

                    visitors::map_instruction_lvalues(instr, |lvalue| {
                        if stable_destructure_source
                            && is_stable_type_identifier(&lvalue.identifier)
                        {
                            return;
                        }
                        if mark_reactive_identifier_id(
                            &mut reactive_ids,
                            &alias_roots,
                            lvalue.identifier.id,
                        ) {
                            debug_reactivity_mark("lvalue", &lvalue.identifier, true);
                            changed = true;
                        }
                        lvalue.reactive = true;
                    });
                }

                // Mutation with reactive input/control can make mutable operands reactive.
                // This mirrors upstream behavior in InferReactivePlaces.
                if has_reactive_input || has_reactive_control {
                    let instr_id = instr.id;
                    visitors::map_instruction_operands(instr, |place| {
                        if is_mutable_effect(place.effect) && is_place_mutable_at(instr_id, place) {
                            if mark_reactive_identifier_id(
                                &mut reactive_ids,
                                &alias_roots,
                                place.identifier.id,
                            ) {
                                debug_reactivity_mark("mutable_operand", &place.identifier, true);
                                changed = true;
                            }
                            place.reactive = true;
                        }
                    });
                }

                // LoadLocal propagation: if the source is reactive, the result is reactive
                if let InstructionValue::LoadLocal { place, .. } = &mut instr.value
                    && is_reactive_identifier_id(&reactive_ids, &alias_roots, place.identifier.id)
                {
                    place.reactive = true;
                    if mark_reactive_identifier_id(
                        &mut reactive_ids,
                        &alias_roots,
                        instr.lvalue.identifier.id,
                    ) {
                        debug_reactivity_mark("load_local_result", &instr.lvalue.identifier, true);
                        changed = true;
                    }
                    instr.lvalue.reactive = true;
                }

                // StoreLocal propagation: if the value is reactive, mark the target
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &mut instr.value
                    && is_reactive_identifier_id(&reactive_ids, &alias_roots, value.identifier.id)
                {
                    value.reactive = true;
                    if mark_reactive_identifier_id(
                        &mut reactive_ids,
                        &alias_roots,
                        lvalue.place.identifier.id,
                    ) {
                        debug_reactivity_mark("store_local_target", &lvalue.place.identifier, true);
                        changed = true;
                    }
                    lvalue.place.reactive = true;
                }
            }

            // Process terminal operands
            visitors::map_terminal_operands(&mut block.terminal, |place| {
                if is_reactive_identifier_id(&reactive_ids, &alias_roots, place.identifier.id) {
                    place.reactive = true;
                }
            });
        }

        if !changed {
            break;
        }
    }

    propagate_reactivity_to_inner_functions(func, &reactive_ids, &alias_roots, true);

    !reactive_ids.is_empty()
}

#[inline]
fn canonical_identifier_id(
    alias_roots: &HashMap<IdentifierId, IdentifierId>,
    id: IdentifierId,
) -> IdentifierId {
    alias_roots.get(&id).copied().unwrap_or(id)
}

#[inline]
fn is_reactive_identifier_id(
    reactive_ids: &HashSet<IdentifierId>,
    alias_roots: &HashMap<IdentifierId, IdentifierId>,
    id: IdentifierId,
) -> bool {
    reactive_ids.contains(&canonical_identifier_id(alias_roots, id))
}

#[inline]
fn mark_reactive_identifier_id(
    reactive_ids: &mut HashSet<IdentifierId>,
    alias_roots: &HashMap<IdentifierId, IdentifierId>,
    id: IdentifierId,
) -> bool {
    reactive_ids.insert(canonical_identifier_id(alias_roots, id))
}

fn debug_reactivity_mark(reason: &str, identifier: &Identifier, inserted: bool) {
    if !inserted || std::env::var("DEBUG_INFER_REACTIVE_FLOW").is_err() {
        return;
    }
    eprintln!(
        "[INFER_REACTIVE] reason={} id={} decl={} name={}",
        reason,
        identifier.id.0,
        identifier.declaration_id.0,
        identifier
            .name
            .as_ref()
            .map(|name| name.value().to_string())
            .unwrap_or_else(|| "<unnamed>".to_string())
    );
}

fn collect_builtin_use_ref_ids(func: &HIRFunction) -> HashSet<IdentifierId> {
    fn is_builtin_use_ref(identifier: &Identifier) -> bool {
        matches!(
            &identifier.type_,
            Type::Object {
                shape_id: Some(shape),
            } if shape == "BuiltInUseRefId"
        )
    }

    fn maybe_insert(set: &mut HashSet<IdentifierId>, identifier: &Identifier) {
        if is_builtin_use_ref(identifier) {
            set.insert(identifier.id);
        }
    }

    let mut ids = HashSet::new();
    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                maybe_insert(&mut ids, &place.identifier);
            }
        }
    }

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            maybe_insert(&mut ids, &phi.place.identifier);
            for operand in phi.operands.values() {
                maybe_insert(&mut ids, &operand.identifier);
            }
        }
        for instr in &block.instructions {
            maybe_insert(&mut ids, &instr.lvalue.identifier);
            visitors::for_each_instruction_operand(instr, |place| {
                maybe_insert(&mut ids, &place.identifier);
            });
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    maybe_insert(&mut ids, &lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    visitors::for_each_pattern_place(&lvalue.pattern, &mut |place| {
                        maybe_insert(&mut ids, &place.identifier);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    maybe_insert(&mut ids, &lvalue.identifier);
                }
                _ => {}
            }
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            maybe_insert(&mut ids, &place.identifier);
        });
    }

    ids
}

fn build_block_index(func: &HIRFunction) -> HashMap<BlockId, usize> {
    let mut block_index: HashMap<BlockId, usize> = HashMap::new();
    for (idx, (id, _)) in func.body.blocks.iter().enumerate() {
        block_index.insert(*id, idx);
    }
    block_index
}

fn block_by_id<'a>(
    func: &'a HIRFunction,
    block_index: &HashMap<BlockId, usize>,
    id: BlockId,
) -> Option<&'a BasicBlock> {
    block_index
        .get(&id)
        .and_then(|idx| func.body.blocks.get(*idx))
        .map(|(_, block)| block)
}

fn post_dominators_of(
    func: &HIRFunction,
    post_dominators: &PostDominator,
    target_id: BlockId,
    block_index: &HashMap<BlockId, usize>,
) -> HashSet<BlockId> {
    let mut result: HashSet<BlockId> = HashSet::new();
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut queue: Vec<BlockId> = vec![target_id];

    while let Some(current_id) = queue.pop() {
        if !visited.insert(current_id) {
            continue;
        }
        let Some(current_block) = block_by_id(func, block_index, current_id) else {
            continue;
        };

        for pred in &current_block.preds {
            let pred_post_dominator = post_dominators.get(*pred).unwrap_or(*pred);
            if pred_post_dominator == target_id || result.contains(&pred_post_dominator) {
                result.insert(*pred);
            }
            queue.push(*pred);
        }
    }

    result
}

fn post_dominator_frontier(
    func: &HIRFunction,
    post_dominators: &PostDominator,
    target_id: BlockId,
    block_index: &HashMap<BlockId, usize>,
) -> HashSet<BlockId> {
    let mut visited: HashSet<BlockId> = HashSet::new();
    let mut frontier: HashSet<BlockId> = HashSet::new();
    let target_post_dominators = post_dominators_of(func, post_dominators, target_id, block_index);

    for block_id in target_post_dominators
        .iter()
        .copied()
        .chain(std::iter::once(target_id))
    {
        if !visited.insert(block_id) {
            continue;
        }
        let Some(block) = block_by_id(func, block_index, block_id) else {
            continue;
        };
        for pred in &block.preds {
            if !target_post_dominators.contains(pred) {
                frontier.insert(*pred);
            }
        }
    }

    frontier
}

fn is_reactive_controlled_block(
    id: BlockId,
    func: &HIRFunction,
    post_dominators: &PostDominator,
    block_index: &HashMap<BlockId, usize>,
    post_dominator_frontier_cache: &mut HashMap<BlockId, HashSet<BlockId>>,
    reactive_ids: &HashSet<IdentifierId>,
    alias_roots: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    let control_blocks = post_dominator_frontier_cache
        .entry(id)
        .or_insert_with(|| post_dominator_frontier(func, post_dominators, id, block_index));

    for control_block_id in control_blocks.iter() {
        let Some(control_block) = block_by_id(func, block_index, *control_block_id) else {
            continue;
        };
        match &control_block.terminal {
            Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
                if is_reactive_identifier_id(reactive_ids, alias_roots, test.identifier.id) {
                    return true;
                }
            }
            Terminal::Switch { test, cases, .. } => {
                if is_reactive_identifier_id(reactive_ids, alias_roots, test.identifier.id)
                    || cases.iter().any(|case| {
                        case.test.as_ref().is_some_and(|test| {
                            is_reactive_identifier_id(reactive_ids, alias_roots, test.identifier.id)
                        })
                    })
                {
                    return true;
                }
            }
            _ => {}
        }
    }

    false
}

fn is_mutable_effect(effect: Effect) -> bool {
    matches!(
        effect,
        Effect::Capture
            | Effect::Store
            | Effect::ConditionallyMutate
            | Effect::ConditionallyMutateIterator
            | Effect::Mutate
    )
}

fn is_place_mutable_at(instr_id: InstructionId, place: &Place) -> bool {
    let range = &place.identifier.mutable_range;
    instr_id.0 >= range.start.0 && instr_id.0 < range.end.0
}

fn propagate_reactivity_to_inner_functions(
    func: &mut HIRFunction,
    reactive_ids: &HashSet<IdentifierId>,
    alias_roots: &HashMap<IdentifierId, IdentifierId>,
    is_outermost: bool,
) {
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            if !is_outermost {
                visitors::map_instruction_operands(instr, |place| {
                    if is_reactive_identifier_id(reactive_ids, alias_roots, place.identifier.id) {
                        place.reactive = true;
                    }
                });
            }

            match &mut instr.value {
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    propagate_reactivity_to_inner_functions(
                        &mut lowered_func.func,
                        reactive_ids,
                        alias_roots,
                        false,
                    );
                }
                _ => {}
            }
        }

        if !is_outermost {
            visitors::map_terminal_operands(&mut block.terminal, |place| {
                if is_reactive_identifier_id(reactive_ids, alias_roots, place.identifier.id) {
                    place.reactive = true;
                }
            });
        }
    }
}

/// Check if a place refers to a hook (function starting with "use" + uppercase).
fn is_hook_identifier(place: &Place) -> bool {
    let name = match &place.identifier.name {
        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => n.as_str(),
        None => return false,
    };
    is_hook_name(name)
}

fn is_hook_name(name: &str) -> bool {
    let candidate = normalize_hook_name(name);
    candidate.starts_with("use")
        && candidate.len() > 3
        && candidate.chars().nth(3).is_some_and(|c| c.is_uppercase())
}

fn is_use_operator_identifier(place: &Place) -> bool {
    let name = match &place.identifier.name {
        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => n.as_str(),
        None => return false,
    };
    is_use_operator_name(name)
}

fn is_use_operator_name(name: &str) -> bool {
    normalize_hook_name(name) == "use"
}

/// Check if a hook returns a stable (non-reactive) value.
/// useRef returns a stable ref object that is the same across renders.
fn is_stable_hook(place: &Place) -> bool {
    let name = match &place.identifier.name {
        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => n.as_str(),
        None => return false,
    };
    matches!(normalize_hook_name(name), "useRef")
}

fn normalize_hook_name(name: &str) -> &str {
    let tail = name.rsplit_once('.').map_or(name, |(_, tail)| tail);
    tail.rsplit_once('$').map_or(tail, |(_, tail)| tail)
}

fn identifier_shape_id(identifier: &Identifier) -> Option<&str> {
    match &identifier.type_ {
        Type::Object {
            shape_id: Some(shape),
        }
        | Type::Function {
            shape_id: Some(shape),
            ..
        } => Some(shape.as_str()),
        _ => None,
    }
}

fn is_stable_type_identifier(identifier: &Identifier) -> bool {
    matches!(
        identifier_shape_id(identifier),
        Some(
            "BuiltInSetState"
                | "BuiltInSetActionState"
                | "BuiltInDispatch"
                | "BuiltInUseRefId"
                | "BuiltInStartTransition"
        )
    )
}

fn is_stable_type_container_identifier(identifier: &Identifier) -> bool {
    matches!(
        &identifier.type_,
        Type::Object {
            shape_id: Some(shape),
        } if matches!(
            shape.as_str(),
            "BuiltInUseState"
                | "BuiltInUseReducer"
                | "BuiltInUseActionState"
                | "BuiltInUseTransition"
                | "BuiltInUseStateHookResult"
                | "BuiltInUseReducerHookResult"
                | "BuiltInUseActionStateHookResult"
                | "BuiltInUseTransitionHookResult"
        )
    )
}
