//! Prune scopes whose dependencies always change.
//!
//! Port of `PruneAlwaysInvalidatingScopes.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Some instructions always produce a new value (allocations like arrays, objects,
//! JSX, new expressions). If such a value is NOT memoized (not within any scope),
//! then any scope that depends on it will always invalidate. This pass prunes
//! such scopes to avoid wasted comparisons.
//!
//! NOTE: function calls are an edge-case. They MAY return primitives, so this
//! pass optimistically assumes they do. Only guaranteed new allocations cause pruning.

use std::collections::HashSet;

use crate::hir::types::*;

fn debug_scope_prune(scope_id: ScopeId) {
    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
        eprintln!(
            "[SCOPE_PRUNE_REASON] scope={} pass=prune_always_invalidating_scopes reason=always-invalidating",
            scope_id.0
        );
    }
}

fn debug_invalidating_mark(
    mark: &str,
    id: IdentifierId,
    within_scope: bool,
    source: Option<IdentifierId>,
) {
    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
        eprintln!(
            "[PRUNE_INVALIDATING_MARK] mark={} id={} within_scope={} source={:?}",
            mark,
            id.0,
            within_scope,
            source.map(|src| src.0)
        );
    }
}

/// Removes scopes whose dependencies always change (always-invalidating).
///
/// Walks the reactive function tree, tracking which identifiers always produce
/// new allocations and which are unmemoized (not within any reactive scope).
/// Scopes depending on unmemoized always-invalidating values are pruned.
pub fn prune_always_invalidating_scopes(func: &mut ReactiveFunction) {
    if std::env::var("DISABLE_PRUNE_ALWAYS_INVALIDATING_SCOPES").is_ok() {
        return;
    }
    let mut state = AlwaysInvalidatingState {
        always_invalidating: HashSet::new(),
        unmemoized: HashSet::new(),
    };
    transform_block(&mut func.body, &mut state, false);
}

struct AlwaysInvalidatingState {
    /// Identifiers whose values are guaranteed to be new allocations.
    always_invalidating: HashSet<IdentifierId>,
    /// Subset of always_invalidating that are NOT within any reactive scope
    /// (i.e. unmemoized and will always change between renders).
    unmemoized: HashSet<IdentifierId>,
}

fn transform_block(
    block: &mut ReactiveBlock,
    state: &mut AlwaysInvalidatingState,
    within_scope: bool,
) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(instr) => {
                process_instruction(instr, state, within_scope);
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(&mut term_stmt.terminal, state, within_scope);
                i += 1;
            }
            ReactiveStatement::Scope(_) => {
                // Take the scope out to inspect and potentially transform
                let stmt = std::mem::replace(
                    &mut block[i],
                    ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                        id: InstructionId(0),
                        lvalue: None,
                        value: InstructionValue::Debugger {
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                    })),
                );

                if let ReactiveStatement::Scope(mut scope_block) = stmt {
                    // Recurse into scope instructions (within_scope = true)
                    transform_block(&mut scope_block.instructions, state, true);

                    // Check if any dependency is an unmemoized always-invalidating value
                    let should_prune = scope_block
                        .scope
                        .dependencies
                        .iter()
                        .any(|dep| state.unmemoized.contains(&dep.identifier.id));

                    if should_prune {
                        if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
                            let mut deps: Vec<u32> = scope_block
                                .scope
                                .dependencies
                                .iter()
                                .map(|dep| dep.identifier.id.0)
                                .collect();
                            deps.sort_unstable();
                            let mut unmemoized: Vec<u32> =
                                state.unmemoized.iter().map(|id| id.0).collect();
                            unmemoized.sort_unstable();
                            let mut matching: Vec<u32> = scope_block
                                .scope
                                .dependencies
                                .iter()
                                .filter_map(|dep| {
                                    state
                                        .unmemoized
                                        .contains(&dep.identifier.id)
                                        .then_some(dep.identifier.id.0)
                                })
                                .collect();
                            matching.sort_unstable();
                            eprintln!(
                                "[SCOPE_PRUNE_REASON] scope={} pass=prune_always_invalidating_scopes deps={:?} matching_unmemoized={:?} unmemoized_all={:?}",
                                scope_block.scope.id.0, deps, matching, unmemoized
                            );
                        }
                        debug_scope_prune(scope_block.scope.id);
                        // Propagate: declarations that are always-invalidating become unmemoized
                        for decl in scope_block.scope.declarations.values() {
                            if state.always_invalidating.contains(&decl.identifier.id) {
                                state.unmemoized.insert(decl.identifier.id);
                                debug_invalidating_mark(
                                    "unmemoized-from-pruned-scope-decl",
                                    decl.identifier.id,
                                    true,
                                    None,
                                );
                            }
                        }
                        for ident in &scope_block.scope.reassignments {
                            if state.always_invalidating.contains(&ident.id) {
                                state.unmemoized.insert(ident.id);
                                debug_invalidating_mark(
                                    "unmemoized-from-pruned-scope-reassignment",
                                    ident.id,
                                    true,
                                    None,
                                );
                            }
                        }

                        block[i] = ReactiveStatement::PrunedScope(PrunedReactiveScopeBlock {
                            scope: scope_block.scope,
                            instructions: scope_block.instructions,
                        });
                    } else {
                        block[i] = ReactiveStatement::Scope(scope_block);
                    }
                }
                i += 1;
            }
            ReactiveStatement::PrunedScope(scope) => {
                transform_block(&mut scope.instructions, state, within_scope);
                i += 1;
            }
        }
    }
}

fn process_instruction(
    instr: &mut ReactiveInstruction,
    state: &mut AlwaysInvalidatingState,
    within_scope: bool,
) {
    let lvalue_id = instr.lvalue.as_ref().map(|lv| lv.identifier.id);

    match &instr.value {
        // These always produce new allocations
        InstructionValue::ArrayExpression { .. }
        | InstructionValue::ObjectExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::NewExpression { .. } => {
            if let Some(id) = lvalue_id {
                state.always_invalidating.insert(id);
                debug_invalidating_mark("always-invalidating", id, within_scope, None);
                if !within_scope {
                    state.unmemoized.insert(id);
                    debug_invalidating_mark("unmemoized", id, within_scope, None);
                }
            }
        }
        // Propagate through StoreLocal
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            if state.always_invalidating.contains(&value.identifier.id) {
                state.always_invalidating.insert(lvalue.place.identifier.id);
                debug_invalidating_mark(
                    "always-invalidating-via-store-local",
                    lvalue.place.identifier.id,
                    within_scope,
                    Some(value.identifier.id),
                );
            }
            if state.unmemoized.contains(&value.identifier.id) {
                state.unmemoized.insert(lvalue.place.identifier.id);
                debug_invalidating_mark(
                    "unmemoized-via-store-local",
                    lvalue.place.identifier.id,
                    within_scope,
                    Some(value.identifier.id),
                );
            }
        }
        // Propagate through LoadLocal
        InstructionValue::LoadLocal { place, .. } => {
            if let Some(id) = lvalue_id {
                if state.always_invalidating.contains(&place.identifier.id) {
                    state.always_invalidating.insert(id);
                    debug_invalidating_mark(
                        "always-invalidating-via-load-local",
                        id,
                        within_scope,
                        Some(place.identifier.id),
                    );
                }
                if state.unmemoized.contains(&place.identifier.id) {
                    state.unmemoized.insert(id);
                    debug_invalidating_mark(
                        "unmemoized-via-load-local",
                        id,
                        within_scope,
                        Some(place.identifier.id),
                    );
                }
            }
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
                let arg_ids = args
                    .iter()
                    .map(|arg| match arg {
                        Argument::Place(p) | Argument::Spread(p) => p.identifier.id.0,
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "[PRUNE_INVALIDATING_CALL] kind=call lvalue={:?} within_scope={} callee_id={} callee_effect={:?} args={:?}",
                    lvalue_id.map(|id| id.0),
                    within_scope,
                    callee.identifier.id.0,
                    callee.effect,
                    arg_ids
                );
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
                let arg_ids = args
                    .iter()
                    .map(|arg| match arg {
                        Argument::Place(p) | Argument::Spread(p) => p.identifier.id.0,
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "[PRUNE_INVALIDATING_CALL] kind=method lvalue={:?} within_scope={} receiver_id={} receiver_effect={:?} property_id={} property_effect={:?} args={:?}",
                    lvalue_id.map(|id| id.0),
                    within_scope,
                    receiver.identifier.id.0,
                    receiver.effect,
                    property.identifier.id.0,
                    property.effect,
                    arg_ids
                );
            }
        }
        _ => {}
    }
}

fn transform_terminal(
    terminal: &mut ReactiveTerminal,
    state: &mut AlwaysInvalidatingState,
    within_scope: bool,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent, state, within_scope);
            if let Some(alt) = alternate {
                transform_block(alt, state, within_scope);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block, state, within_scope);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block, state, within_scope);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init, state, within_scope);
            if let Some(upd) = update {
                transform_block(upd, state, within_scope);
            }
            transform_block(loop_block, state, within_scope);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init, state, within_scope);
            transform_block(loop_block, state, within_scope);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init, state, within_scope);
            transform_block(loop_block, state, within_scope);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block, state, within_scope);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block, state, within_scope);
            transform_block(handler, state, within_scope);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_identifier(id: u32, name: Option<IdentifierName>) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32) -> Place {
        Place {
            identifier: make_identifier(id, None),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_scope(id: u32, dep_ids: Vec<u32>) -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(id),
            range: MutableRange::default(),
            dependencies: dep_ids
                .into_iter()
                .map(|did| ReactiveScopeDependency {
                    identifier: make_identifier(did, None),
                    path: vec![],
                })
                .collect(),
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_prune_scope_depending_on_unmemoized_allocation() {
        // Instruction outside scope creates an array (always-invalidating + unmemoized)
        // Scope depends on that value -> should be pruned
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                // Array expression outside any scope (unmemoized)
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1)),
                    value: InstructionValue::ArrayExpression {
                        elements: vec![],
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                // Scope that depends on the array
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_scope(10, vec![1]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place(2)),
                            value: InstructionValue::Primitive {
                                value: PrimitiveValue::Number(42.0),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
            ],
            directives: vec![],
        };

        prune_always_invalidating_scopes(&mut func);

        assert_eq!(func.body.len(), 2);
        assert!(
            matches!(&func.body[1], ReactiveStatement::PrunedScope(_)),
            "Scope depending on unmemoized always-invalidating value should be pruned"
        );
    }

    #[test]
    fn test_keep_scope_with_memoized_allocation() {
        // Array expression INSIDE a scope (memoized) — dependent scope should NOT be pruned
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                // Scope containing an array expression (memoized)
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_scope(10, vec![]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(0),
                            lvalue: Some(make_place(1)),
                            value: InstructionValue::ArrayExpression {
                                elements: vec![],
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
                // Another scope depending on the memoized array
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_scope(20, vec![1]),
                    instructions: vec![],
                }),
            ],
            directives: vec![],
        };

        prune_always_invalidating_scopes(&mut func);

        // The second scope should NOT be pruned because the array is memoized
        assert!(
            matches!(&func.body[1], ReactiveStatement::Scope(_)),
            "Scope depending on memoized always-invalidating value should be kept"
        );
    }

    #[test]
    fn test_no_scopes_nothing_to_prune() {
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Instruction(Box::new(
                ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: None,
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
            directives: vec![],
        };

        prune_always_invalidating_scopes(&mut func);
        assert_eq!(func.body.len(), 1);
    }
}
