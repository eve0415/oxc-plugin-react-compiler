//! Compute unconditional blocks.
//!
//! Port of `ComputeUnconditionalBlocks.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Returns the set of blocks that are unconditionally executed (always reached
//! from the entry block). Uses post-dominator analysis — specifically the chain
//! of post-dominators from the entry block forms the set of blocks that every
//! execution path must pass through.

use std::collections::HashSet;

use super::dominator::{PostDominatorOptions, compute_post_dominator_tree};
use super::types::*;

/// Compute the set of blocks that are always reachable from the entry block.
///
/// This walks the post-dominator chain starting from the entry block. A block
/// that post-dominates the entry is guaranteed to execute on every path that
/// returns normally (throws are not considered exit nodes).
pub fn compute_unconditional_blocks(func: &HIRFunction) -> HashSet<BlockId> {
    let mut unconditional_blocks = HashSet::new();

    let dominators = compute_post_dominator_tree(
        func,
        PostDominatorOptions {
            // Hooks must only be in a consistent order for executions that
            // return normally, so we opt-in to viewing throw as a non-exit node.
            include_throws_as_exit_node: false,
        },
    );

    let exit = dominators.exit();
    let mut current: Option<BlockId> = Some(func.body.entry);

    while let Some(block_id) = current {
        if block_id == exit {
            break;
        }
        assert!(
            !unconditional_blocks.contains(&block_id),
            "Internal error: non-terminating loop in compute_unconditional_blocks"
        );
        unconditional_blocks.insert(block_id);
        current = dominators.get(block_id);
    }

    unconditional_blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_place() -> Place {
        Place {
            identifier: make_temporary_identifier(IdentifierId(0), SourceLocation::Generated),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_test_function(blocks: Vec<(u32, HashSet<u32>, Terminal)>) -> HIRFunction {
        let entry = BlockId(blocks[0].0);
        let body_blocks: Vec<(BlockId, BasicBlock)> = blocks
            .into_iter()
            .map(|(id, preds, terminal)| {
                let block_id = BlockId(id);
                let pred_set: HashSet<BlockId> = preds.into_iter().map(BlockId).collect();
                (
                    block_id,
                    BasicBlock {
                        kind: BlockKind::Block,
                        id: block_id,
                        instructions: vec![],
                        terminal,
                        preds: pred_set,
                        phis: vec![],
                    },
                )
            })
            .collect();

        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(),
            context: vec![],
            body: HIR {
                entry,
                blocks: body_blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_linear_all_unconditional() {
        // bb0 -> bb1 -> bb2 (return)
        // All blocks are unconditional since there's only one path.
        let func = make_test_function(vec![
            (
                0,
                HashSet::new(),
                Terminal::Goto {
                    block: BlockId(1),
                    variant: GotoVariant::Break,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
            (
                1,
                HashSet::from([0]),
                Terminal::Goto {
                    block: BlockId(2),
                    variant: GotoVariant::Break,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
            (
                2,
                HashSet::from([1]),
                Terminal::Return {
                    value: make_place(),
                    return_variant: ReturnVariant::Explicit,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
        ]);

        let unconditional = compute_unconditional_blocks(&func);
        assert!(unconditional.contains(&BlockId(0)));
        assert!(unconditional.contains(&BlockId(1)));
        assert!(unconditional.contains(&BlockId(2)));
    }

    #[test]
    fn test_diamond_unconditional() {
        // bb0 -> bb1, bb2 (if)
        // bb1 -> bb3
        // bb2 -> bb3
        // bb3 -> return
        // Only bb0 and bb3 are unconditional (bb1 and bb2 are conditional branches).
        let func = make_test_function(vec![
            (
                0,
                HashSet::new(),
                Terminal::If {
                    test: make_place(),
                    loc: SourceLocation::Generated,
                    consequent: BlockId(1),
                    alternate: BlockId(2),
                    fallthrough: BlockId(3),
                    id: InstructionId(0),
                },
            ),
            (
                1,
                HashSet::from([0]),
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
            (
                2,
                HashSet::from([0]),
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
            (
                3,
                HashSet::from([1, 2]),
                Terminal::Return {
                    value: make_place(),
                    return_variant: ReturnVariant::Explicit,
                    loc: SourceLocation::Generated,
                    id: InstructionId(0),
                },
            ),
        ]);

        let unconditional = compute_unconditional_blocks(&func);
        assert!(unconditional.contains(&BlockId(0)));
        assert!(!unconditional.contains(&BlockId(1)));
        assert!(!unconditional.contains(&BlockId(2)));
        assert!(unconditional.contains(&BlockId(3)));
    }
}
