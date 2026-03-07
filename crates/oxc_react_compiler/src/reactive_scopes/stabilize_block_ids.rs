//! Stabilize block IDs for deterministic output.
//!
//! Port of `StabilizeBlockIds.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Renumbers block IDs sequentially starting from 0 based on the order
//! they are first encountered, ensuring deterministic output regardless
//! of the internal compilation order.

use std::collections::HashMap;

use crate::hir::types::*;

/// Renumbers all block IDs sequentially for deterministic output.
/// First collects all referenced label IDs (non-implicit), then rewrites
/// all block ID references to use sequential numbering.
pub fn stabilize_block_ids(func: &mut ReactiveFunction) {
    // Pass 1: Collect referenced (non-implicit) label IDs in order
    let mut referenced: Vec<BlockId> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    collect_referenced_labels(&func.body, &mut referenced, &mut seen);

    // Build mapping from old ID -> new sequential ID
    let mut mappings: HashMap<BlockId, BlockId> = HashMap::new();
    for block_id in &referenced {
        let next_id = mappings.len() as u32;
        mappings.entry(*block_id).or_insert(BlockId(next_id));
    }

    // Pass 2: Rewrite all block IDs
    rewrite_block(&mut func.body, &mut mappings);
}

fn collect_referenced_labels(
    block: &ReactiveBlock,
    referenced: &mut Vec<BlockId>,
    seen: &mut std::collections::HashSet<BlockId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(_) => {}
            ReactiveStatement::Terminal(term_stmt) => {
                // Collect label ID if non-implicit
                if let Some(label) = &term_stmt.label
                    && !label.implicit
                    && seen.insert(label.id)
                {
                    referenced.push(label.id);
                }
                collect_from_terminal(&term_stmt.terminal, referenced, seen);
            }
            ReactiveStatement::Scope(scope) => {
                // Collect early return value label if present
                if let Some(ref erv) = scope.scope.early_return_value
                    && seen.insert(erv.label)
                {
                    referenced.push(erv.label);
                }
                collect_referenced_labels(&scope.instructions, referenced, seen);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_referenced_labels(&scope.instructions, referenced, seen);
            }
        }
    }
}

fn collect_from_terminal(
    terminal: &ReactiveTerminal,
    referenced: &mut Vec<BlockId>,
    seen: &mut std::collections::HashSet<BlockId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_referenced_labels(consequent, referenced, seen);
            if let Some(alt) = alternate {
                collect_referenced_labels(alt, referenced, seen);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_referenced_labels(block, referenced, seen);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_referenced_labels(loop_block, referenced, seen);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_referenced_labels(init, referenced, seen);
            if let Some(upd) = update {
                collect_referenced_labels(upd, referenced, seen);
            }
            collect_referenced_labels(loop_block, referenced, seen);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            collect_referenced_labels(init, referenced, seen);
            collect_referenced_labels(loop_block, referenced, seen);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_referenced_labels(init, referenced, seen);
            collect_referenced_labels(loop_block, referenced, seen);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_referenced_labels(block, referenced, seen);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_referenced_labels(block, referenced, seen);
            collect_referenced_labels(handler, referenced, seen);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn rewrite_block(block: &mut ReactiveBlock, mappings: &mut HashMap<BlockId, BlockId>) {
    for stmt in block.iter_mut() {
        match stmt {
            ReactiveStatement::Instruction(_) => {}
            ReactiveStatement::Terminal(term_stmt) => {
                // Rewrite label ID
                if let Some(label) = &mut term_stmt.label {
                    let new_id = get_or_insert(mappings, label.id);
                    label.id = new_id;
                }
                rewrite_terminal(&mut term_stmt.terminal, mappings);
            }
            ReactiveStatement::Scope(scope) => {
                // Rewrite early return value label
                if let Some(ref mut erv) = scope.scope.early_return_value {
                    erv.label = get_or_insert(mappings, erv.label);
                }
                rewrite_block(&mut scope.instructions, mappings);
            }
            ReactiveStatement::PrunedScope(scope) => {
                rewrite_block(&mut scope.instructions, mappings);
            }
        }
    }
}

fn rewrite_terminal(terminal: &mut ReactiveTerminal, mappings: &mut HashMap<BlockId, BlockId>) {
    match terminal {
        ReactiveTerminal::Break { target, .. } | ReactiveTerminal::Continue { target, .. } => {
            *target = get_or_insert(mappings, *target);
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            rewrite_block(consequent, mappings);
            if let Some(alt) = alternate {
                rewrite_block(alt, mappings);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    rewrite_block(block, mappings);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            rewrite_block(loop_block, mappings);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            rewrite_block(init, mappings);
            if let Some(upd) = update {
                rewrite_block(upd, mappings);
            }
            rewrite_block(loop_block, mappings);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            rewrite_block(init, mappings);
            rewrite_block(loop_block, mappings);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            rewrite_block(init, mappings);
            rewrite_block(loop_block, mappings);
        }
        ReactiveTerminal::Label { block, .. } => {
            rewrite_block(block, mappings);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            rewrite_block(block, mappings);
            rewrite_block(handler, mappings);
        }
        ReactiveTerminal::Return { .. } | ReactiveTerminal::Throw { .. } => {}
    }
}

/// Get the mapped ID or insert a new sequential one.
fn get_or_insert(mappings: &mut HashMap<BlockId, BlockId>, id: BlockId) -> BlockId {
    let next = mappings.len() as u32;
    *mappings.entry(id).or_insert(BlockId(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stabilize_block_ids_sequential() {
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Label {
                        block: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                            terminal: ReactiveTerminal::Break {
                                target: BlockId(100),
                                target_kind: ReactiveTerminalTargetKind::Labeled,
                                id: InstructionId(2),
                                loc: SourceLocation::Generated,
                            },
                            label: None,
                        })],
                        id: InstructionId(0),
                        loc: SourceLocation::Generated,
                    },
                    label: Some(ReactiveLabel {
                        id: BlockId(100),
                        implicit: false,
                    }),
                }),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Label {
                        block: vec![],
                        id: InstructionId(3),
                        loc: SourceLocation::Generated,
                    },
                    label: Some(ReactiveLabel {
                        id: BlockId(200),
                        implicit: false,
                    }),
                }),
            ],
            directives: vec![],
        };

        stabilize_block_ids(&mut func);

        // First label should be remapped to 0
        if let ReactiveStatement::Terminal(term) = &func.body[0] {
            assert_eq!(term.label.as_ref().unwrap().id, BlockId(0));
            // Break target should also be remapped to 0
            if let ReactiveTerminal::Label { block, .. } = &term.terminal
                && let ReactiveStatement::Terminal(inner) = &block[0]
                && let ReactiveTerminal::Break { target, .. } = &inner.terminal
            {
                assert_eq!(*target, BlockId(0));
            }
        }

        // Second label should be remapped to 1
        if let ReactiveStatement::Terminal(term) = &func.body[1] {
            assert_eq!(term.label.as_ref().unwrap().id, BlockId(1));
        }
    }

    #[test]
    fn test_stabilize_empty_function() {
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![],
            directives: vec![],
        };

        stabilize_block_ids(&mut func);
        assert!(func.body.is_empty());
    }
}
