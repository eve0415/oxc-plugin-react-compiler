//! Prune unused labels from the reactive function tree.
//!
//! Port of `PruneUnusedLabels.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Flattens labeled terminals where the label is not reachable via
//! break/continue, and marks labels as implicit when they are unused.

use std::collections::HashSet;

use crate::hir::types::*;

/// Removes Label terminals that are never targeted by break/continue.
/// Labels that are unreachable are flattened (their block is inlined).
/// Labels that exist but are unreferenced get marked as `implicit`.
pub fn prune_unused_labels(func: &mut ReactiveFunction) {
    let mut labels: HashSet<BlockId> = HashSet::new();
    // First pass: collect all targeted label IDs
    collect_targeted_labels(&func.body, &mut labels);
    // Second pass: transform
    transform_block(&mut func.body, &labels);
}

/// Collect all BlockIds that are targets of labeled break/continue statements.
fn collect_targeted_labels(block: &ReactiveBlock, labels: &mut HashSet<BlockId>) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(_) => {}
            ReactiveStatement::Terminal(term_stmt) => {
                collect_from_terminal(&term_stmt.terminal, labels);
            }
            ReactiveStatement::Scope(scope) => {
                collect_targeted_labels(&scope.instructions, labels);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_targeted_labels(&scope.instructions, labels);
            }
        }
    }
}

fn collect_from_terminal(terminal: &ReactiveTerminal, labels: &mut HashSet<BlockId>) {
    match terminal {
        ReactiveTerminal::Break {
            target,
            target_kind: ReactiveTerminalTargetKind::Labeled,
            ..
        }
        | ReactiveTerminal::Continue {
            target,
            target_kind: ReactiveTerminalTargetKind::Labeled,
            ..
        } => {
            labels.insert(*target);
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_targeted_labels(consequent, labels);
            if let Some(alt) = alternate {
                collect_targeted_labels(alt, labels);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_targeted_labels(block, labels);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_targeted_labels(loop_block, labels);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_targeted_labels(init, labels);
            if let Some(upd) = update {
                collect_targeted_labels(upd, labels);
            }
            collect_targeted_labels(loop_block, labels);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            collect_targeted_labels(init, labels);
            collect_targeted_labels(loop_block, labels);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_targeted_labels(init, labels);
            collect_targeted_labels(loop_block, labels);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_targeted_labels(block, labels);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_targeted_labels(block, labels);
            collect_targeted_labels(handler, labels);
        }
        _ => {}
    }
}

/// Transform the block, flattening unreachable label terminals and marking
/// unused labels as implicit.
fn transform_block(block: &mut ReactiveBlock, labels: &HashSet<BlockId>) {
    let mut i = 0;
    while i < block.len() {
        // First, recurse into children
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                i += 1;
                continue;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(&mut term_stmt.terminal, labels);

                let is_reachable_label = term_stmt
                    .label
                    .as_ref()
                    .is_some_and(|l| labels.contains(&l.id));

                if matches!(&term_stmt.terminal, ReactiveTerminal::Label { .. })
                    && !is_reachable_label
                {
                    // Flatten: replace this terminal with its block contents
                    let term_stmt = if let ReactiveStatement::Terminal(t) = block.remove(i) {
                        t
                    } else {
                        unreachable!()
                    };

                    if let ReactiveTerminal::Label {
                        block: mut label_block,
                        ..
                    } = term_stmt.terminal
                    {
                        // Remove trailing implicit break (break with no real target)
                        if let Some(last) = label_block.last()
                            && matches!(
                                last,
                                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                                    terminal: ReactiveTerminal::Break {
                                        target_kind: ReactiveTerminalTargetKind::Implicit,
                                        ..
                                    },
                                    ..
                                })
                            )
                        {
                            label_block.pop();
                        }
                        // Insert the flattened statements
                        let count = label_block.len();
                        for (j, stmt) in label_block.into_iter().enumerate() {
                            block.insert(i + j, stmt);
                        }
                        // Don't increment i — re-process the newly inserted items
                        // But we do need to skip past them eventually
                        i += count;
                    }
                    continue;
                }

                // Not a label terminal, or label is reachable — mark implicit if unused
                if let ReactiveStatement::Terminal(term_stmt) = &mut block[i]
                    && !is_reachable_label
                    && let Some(label) = &mut term_stmt.label
                {
                    label.implicit = true;
                }
            }
            ReactiveStatement::Scope(scope) => {
                transform_block(&mut scope.instructions, labels);
            }
            ReactiveStatement::PrunedScope(scope) => {
                transform_block(&mut scope.instructions, labels);
            }
        }
        i += 1;
    }
}

fn transform_terminal(terminal: &mut ReactiveTerminal, labels: &HashSet<BlockId>) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent, labels);
            if let Some(alt) = alternate {
                transform_block(alt, labels);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block, labels);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block, labels);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init, labels);
            if let Some(upd) = update {
                transform_block(upd, labels);
            }
            transform_block(loop_block, labels);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init, labels);
            transform_block(loop_block, labels);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init, labels);
            transform_block(loop_block, labels);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block, labels);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block, labels);
            transform_block(handler, labels);
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

    #[test]
    fn test_prune_unused_label_flattens() {
        // A label terminal with no break targeting it should be flattened
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Label {
                    block: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: None,
                            value: InstructionValue::Primitive {
                                value: PrimitiveValue::Number(42.0),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                    id: InstructionId(0),
                },
                label: Some(ReactiveLabel {
                    id: BlockId(99),
                    implicit: false,
                }),
            })],
        };

        prune_unused_labels(&mut func);

        // The label should be flattened — we should now have just the instruction
        assert_eq!(func.body.len(), 1);
        assert!(matches!(&func.body[0], ReactiveStatement::Instruction(_)));
    }

    #[test]
    fn test_keep_label_when_targeted() {
        // A label terminal with a break targeting it should be kept
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Label {
                    block: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Break {
                            target: BlockId(10),
                            target_kind: ReactiveTerminalTargetKind::Labeled,
                            id: InstructionId(2),
                        },
                        label: None,
                    })],
                    id: InstructionId(0),
                },
                label: Some(ReactiveLabel {
                    id: BlockId(10),
                    implicit: false,
                }),
            })],
        };

        prune_unused_labels(&mut func);

        // The label should remain since it's targeted
        assert_eq!(func.body.len(), 1);
        assert!(matches!(
            &func.body[0],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Label { .. },
                ..
            })
        ));
    }
}
