//! FlattenReactiveLoopsHIR — remove reactive scope terminals inside loops.
//!
//! Port of `ReactiveScopes/FlattenReactiveLoopsHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Reactive scopes inside loops cannot be memoized as a single reactive block
//! because this would require an extra layer of reconciliation. This pass
//! converts `Scope` terminals that are transitively inside a loop terminal
//! into `PrunedScope` terminals so they are treated as non-memoizable.

use super::types::*;

/// Flatten reactive scope terminals that are directly inside loop terminals.
///
/// The algorithm tracks which loop fallthroughs are still "active" (i.e., we
/// haven't yet reached the fallthrough block). While active loops exist, any
/// `Scope` terminal is rewritten to `PrunedScope`.
///
/// This mirrors the upstream `flattenReactiveLoopsHIR` function.
pub fn flatten_reactive_loops_hir(func: &mut HIRFunction) {
    let mut active_loops: Vec<BlockId> = Vec::new();

    for idx in 0..func.body.blocks.len() {
        let block_id = func.body.blocks[idx].1.id;

        // Remove any active loop whose fallthrough matches the current block.
        // This is equivalent to upstream's `retainWhere(activeLoops, id => id !== block.id)`.
        active_loops.retain(|id| *id != block_id);

        let terminal = &func.body.blocks[idx].1.terminal;
        match terminal {
            // Loop terminals: push their fallthrough onto the active list.
            Terminal::DoWhile { fallthrough, .. }
            | Terminal::For { fallthrough, .. }
            | Terminal::ForIn { fallthrough, .. }
            | Terminal::ForOf { fallthrough, .. }
            | Terminal::While { fallthrough, .. } => {
                active_loops.push(*fallthrough);
            }

            // Scope terminal inside a loop: rewrite to PrunedScope.
            Terminal::Scope { .. } => {
                if !active_loops.is_empty() {
                    // Extract fields from the Scope terminal and replace with PrunedScope.
                    let old_terminal = std::mem::replace(
                        &mut func.body.blocks[idx].1.terminal,
                        Terminal::Unreachable {
                            id: InstructionId::default(),
                            loc: SourceLocation::Generated,
                        },
                    );
                    if let Terminal::Scope {
                        block,
                        fallthrough,
                        scope,
                        id,
                        loc,
                    } = old_terminal
                    {
                        func.body.blocks[idx].1.terminal = Terminal::PrunedScope {
                            block,
                            fallthrough,
                            scope,
                            id,
                            loc,
                        };
                    }
                }
            }

            // All other terminals: no action needed.
            Terminal::Branch { .. }
            | Terminal::Goto { .. }
            | Terminal::If { .. }
            | Terminal::Label { .. }
            | Terminal::Logical { .. }
            | Terminal::MaybeThrow { .. }
            | Terminal::Optional { .. }
            | Terminal::PrunedScope { .. }
            | Terminal::Return { .. }
            | Terminal::Sequence { .. }
            | Terminal::Switch { .. }
            | Terminal::Ternary { .. }
            | Terminal::Throw { .. }
            | Terminal::Try { .. }
            | Terminal::Unreachable { .. }
            | Terminal::Unsupported { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

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

    fn make_func(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn make_scope() -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(1),
            range: MutableRange {
                start: InstructionId(1),
                end: InstructionId(10),
            },
            dependencies: vec![],
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_scope_inside_for_loop_is_pruned() {
        // Block 0: For { ..., fallthrough: 3 }
        // Block 1: Scope { block: 2, fallthrough: 3, scope: ... }
        // Block 2: Goto -> 3
        // Block 3: Return
        //
        // Block 1 has a Scope terminal and is inside the For loop (fallthrough=3 hasn't been reached).
        // After flattening, block 1's terminal should be PrunedScope.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::For {
                        init: BlockId(1),
                        test: BlockId(1),
                        update: None,
                        loop_block: BlockId(1),
                        fallthrough: BlockId(3),
                        id: InstructionId(1),
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
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(2),
                        fallthrough: BlockId(3),
                        scope: make_scope(),
                        id: InstructionId(2),
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
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(3),
                        variant: GotoVariant::Break,
                        id: InstructionId(3),
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
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_reactive_loops_hir(&mut func);

        // Block 1's Scope terminal should now be PrunedScope.
        assert!(
            matches!(func.body.blocks[1].1.terminal, Terminal::PrunedScope { .. }),
            "Expected PrunedScope, got {:?}",
            std::mem::discriminant(&func.body.blocks[1].1.terminal)
        );

        // Block 3 (the fallthrough) should still be Return.
        assert!(matches!(
            func.body.blocks[3].1.terminal,
            Terminal::Return { .. }
        ));
    }

    #[test]
    fn test_scope_outside_loop_not_pruned() {
        // Block 0: Scope { block: 1, fallthrough: 2 }
        // Block 1: Goto -> 2
        // Block 2: Return
        //
        // No loop is active, so the Scope terminal should remain unchanged.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: make_scope(),
                        id: InstructionId(1),
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
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(2),
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
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(3),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_reactive_loops_hir(&mut func);

        // Block 0's Scope terminal should remain Scope (not PrunedScope).
        assert!(
            matches!(func.body.blocks[0].1.terminal, Terminal::Scope { .. }),
            "Expected Scope to remain, got {:?}",
            std::mem::discriminant(&func.body.blocks[0].1.terminal)
        );
    }

    #[test]
    fn test_scope_after_loop_fallthrough_not_pruned() {
        // Block 0: While { ..., fallthrough: 2 }
        // Block 1: Goto -> 0 (loop body)
        // Block 2: Scope { block: 3, fallthrough: 4 } -- after loop
        // Block 3: Goto -> 4
        // Block 4: Return
        //
        // Block 2 appears after the while loop's fallthrough (block 2), so by
        // the time we process block 2, the active loop is removed.
        let mut func = make_func(vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::While {
                        test: BlockId(1),
                        loop_block: BlockId(1),
                        fallthrough: BlockId(2),
                        id: InstructionId(1),
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
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(0),
                        variant: GotoVariant::Continue,
                        id: InstructionId(2),
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
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(3),
                        fallthrough: BlockId(4),
                        scope: make_scope(),
                        id: InstructionId(3),
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
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(4),
                        variant: GotoVariant::Break,
                        id: InstructionId(4),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(4),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(4),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(1),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(5),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ]);

        flatten_reactive_loops_hir(&mut func);

        // Block 2's Scope should remain Scope — the loop is no longer active.
        assert!(
            matches!(func.body.blocks[2].1.terminal, Terminal::Scope { .. }),
            "Expected Scope to remain after loop fallthrough, got {:?}",
            std::mem::discriminant(&func.body.blocks[2].1.terminal)
        );
    }
}
