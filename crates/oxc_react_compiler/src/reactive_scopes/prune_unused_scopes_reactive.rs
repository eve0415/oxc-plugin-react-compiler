//! Prune unused reactive scopes from the reactive function tree.
//!
//! Port of `PruneUnusedScopes.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Converts scopes without outputs (no declarations, no reassignments) into
//! PrunedScope blocks. A scope can also be pruned if it only contains
//! declarations that bubbled up from inner scopes.

use crate::hir::types::*;

fn debug_scope_prune(scope_id: ScopeId) {
    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
        eprintln!(
            "[SCOPE_PRUNE_REASON] scope={} pass=prune_unused_scopes_reactive reason=unused",
            scope_id.0
        );
    }
}

/// Removes empty reactive scope blocks by converting them to PrunedScope.
///
/// A scope is pruned if:
/// - It has no return statement in its body
/// - It has no reassignments
/// - It has no declarations, OR all its declarations came from inner scopes
pub fn prune_unused_scopes(func: &mut ReactiveFunction) {
    transform_block(&mut func.body);
}

/// State tracked per-scope during the transform.
struct ScopeState {
    has_return_statement: bool,
}

fn transform_block(block: &mut ReactiveBlock) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(&mut term_stmt.terminal);
                // Check for return statements after transforming children
                if matches!(&term_stmt.terminal, ReactiveTerminal::Return { .. }) {
                    // Parent scope state will track this
                }
                i += 1;
            }
            ReactiveStatement::Scope(_) => {
                // We need to take the scope out to inspect and transform it
                let stmt = std::mem::replace(
                    &mut block[i],
                    // Temporary placeholder — will be replaced below
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
                    // Recurse into the scope's instructions first
                    let mut state = ScopeState {
                        has_return_statement: false,
                    };
                    transform_block_with_state(&mut scope_block.instructions, &mut state);

                    // Check if this scope should be pruned
                    let has_own_decl = has_own_declaration(&scope_block);
                    let should_prune = !state.has_return_statement
                        && scope_block.scope.reassignments.is_empty()
                        && (scope_block.scope.declarations.is_empty() || !has_own_decl);

                    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
                        let mut decl_scopes: Vec<(u32, u32, u32)> = scope_block
                            .scope
                            .declarations
                            .iter()
                            .map(|(id, decl)| (id.0, decl.scope.id.0, scope_block.scope.id.0))
                            .collect();
                        decl_scopes.sort_by_key(|(id, _, _)| *id);
                        eprintln!(
                            "[SCOPE_PRUNE_REASON] scope={} pass=prune_unused_scopes_reactive state has_return={} has_optional_dep=false reassignments={} declarations={} has_own_decl={} decl_scope_pairs={:?}",
                            scope_block.scope.id.0,
                            state.has_return_statement,
                            scope_block.scope.reassignments.len(),
                            scope_block.scope.declarations.len(),
                            has_own_decl,
                            decl_scopes
                        );
                    }

                    if should_prune {
                        debug_scope_prune(scope_block.scope.id);
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
                transform_block(&mut scope.instructions);
                i += 1;
            }
        }
    }
}

fn transform_block_with_state(block: &mut ReactiveBlock, state: &mut ScopeState) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal_with_state(&mut term_stmt.terminal, state);
                if matches!(&term_stmt.terminal, ReactiveTerminal::Return { .. }) {
                    state.has_return_statement = true;
                }
                i += 1;
            }
            ReactiveStatement::Scope(_) => {
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
                    let mut scope_state = ScopeState {
                        has_return_statement: false,
                    };
                    transform_block_with_state(&mut scope_block.instructions, &mut scope_state);

                    let has_own_decl = has_own_declaration(&scope_block);
                    let should_prune = !scope_state.has_return_statement
                        && scope_block.scope.reassignments.is_empty()
                        && (scope_block.scope.declarations.is_empty() || !has_own_decl);

                    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
                        let mut decl_scopes: Vec<(u32, u32, u32)> = scope_block
                            .scope
                            .declarations
                            .iter()
                            .map(|(id, decl)| (id.0, decl.scope.id.0, scope_block.scope.id.0))
                            .collect();
                        decl_scopes.sort_by_key(|(id, _, _)| *id);
                        eprintln!(
                            "[SCOPE_PRUNE_REASON] scope={} pass=prune_unused_scopes_reactive state has_return={} has_optional_dep=false reassignments={} declarations={} has_own_decl={} decl_scope_pairs={:?}",
                            scope_block.scope.id.0,
                            scope_state.has_return_statement,
                            scope_block.scope.reassignments.len(),
                            scope_block.scope.declarations.len(),
                            has_own_decl,
                            decl_scopes
                        );
                    }

                    if should_prune {
                        debug_scope_prune(scope_block.scope.id);
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
                transform_block(&mut scope.instructions);
                i += 1;
            }
        }
    }
}

fn transform_terminal(terminal: &mut ReactiveTerminal) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent);
            if let Some(alt) = alternate {
                transform_block(alt);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init);
            if let Some(upd) = update {
                transform_block(upd);
            }
            transform_block(loop_block);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init);
            transform_block(loop_block);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init);
            transform_block(loop_block);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block);
            transform_block(handler);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

/// Like `transform_terminal` but propagates return statement detection through state.
/// This matches the upstream behavior where `traverseTerminal` passes the same state
/// to all child blocks, allowing returns inside If/Switch/etc to propagate up.
fn transform_terminal_with_state(terminal: &mut ReactiveTerminal, state: &mut ScopeState) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block_with_state(consequent, state);
            if let Some(alt) = alternate {
                transform_block_with_state(alt, state);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block_with_state(block, state);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block_with_state(loop_block, state);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block_with_state(init, state);
            if let Some(upd) = update {
                transform_block_with_state(upd, state);
            }
            transform_block_with_state(loop_block, state);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block_with_state(init, state);
            transform_block_with_state(loop_block, state);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block_with_state(init, state);
            transform_block_with_state(loop_block, state);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block_with_state(block, state);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block_with_state(block, state);
            transform_block_with_state(handler, state);
        }
        ReactiveTerminal::Return { .. } => {
            state.has_return_statement = true;
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

/// Does the scope block declare any values of its own?
/// Returns false if all the block's declarations are propagated from nested scopes.
///
/// Upstream checks `declaration.scope.id === block.scope.id` where `declaration.scope`
/// is the ReactiveScope in which the variable was originally declared.
fn has_own_declaration(block: &ReactiveScopeBlock) -> bool {
    for declaration in block.scope.declarations.values() {
        if declaration.scope.id == block.scope.id {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_scope(id: u32) -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(id),
            range: MutableRange::default(),
            dependencies: vec![],
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    fn make_identifier(id: u32) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: None,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32) -> Place {
        Place {
            identifier: make_identifier(id),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    #[test]
    fn test_prune_empty_scope() {
        // A scope with no declarations and no reassignments should be pruned
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: make_scope(1),
                instructions: vec![ReactiveStatement::Instruction(Box::new(
                    ReactiveInstruction {
                        id: InstructionId(0),
                        lvalue: None,
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Number(42.0),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                    },
                ))],
            })],
            directives: vec![],
        };

        prune_unused_scopes(&mut func);

        assert_eq!(func.body.len(), 1);
        assert!(
            matches!(&func.body[0], ReactiveStatement::PrunedScope(_)),
            "Empty scope should be converted to PrunedScope"
        );
    }

    #[test]
    fn test_keep_scope_with_reassignments() {
        let mut scope = make_scope(1);
        scope.reassignments.push(make_identifier(10));

        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope,
                instructions: vec![],
            })],
            directives: vec![],
        };

        prune_unused_scopes(&mut func);

        assert!(
            matches!(&func.body[0], ReactiveStatement::Scope(_)),
            "Scope with reassignments should be kept"
        );
    }

    #[test]
    fn test_keep_scope_with_own_declarations() {
        let mut scope = make_scope(1);
        scope.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier(10),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope,
                instructions: vec![],
            })],
            directives: vec![],
        };

        prune_unused_scopes(&mut func);

        // The scope has declarations but has_own_declaration checks scope.id match.
    }

    #[test]
    fn test_keep_scope_with_return() {
        // A scope containing a return statement should be kept
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: make_scope(1),
                instructions: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: make_place(1),
                        id: InstructionId(0),
                        loc: SourceLocation::Generated,
                    },
                    label: None,
                })],
            })],
            directives: vec![],
        };

        prune_unused_scopes(&mut func);

        assert!(
            matches!(&func.body[0], ReactiveStatement::Scope(_)),
            "Scope with return statement should be kept"
        );
    }
}
