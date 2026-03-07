//! Validates that JSX elements don't appear inside try/catch blocks.
//!
//! Port of `ValidateNoJSXInTryStatement.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Developers may not be aware of error boundaries and lazy evaluation of JSX, leading them
//! to use patterns such as `let el; try { el = <Component /> } catch { ... }` to attempt to
//! catch rendering errors. Such code will fail to catch errors in rendering, but developers
//! may not realize this right away.
//!
//! This validation pass validates against this pattern: specifically, it errors for JSX
//! created within a try block. JSX is allowed within a catch statement, unless that catch
//! is itself nested inside an outer try.

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;

/// Validates that no JSX elements (JsxExpression, JsxFragment) appear inside try blocks.
///
/// The upstream logic walks blocks in order and tracks "active try blocks" by
/// pushing the handler block id when a Try terminal is encountered. When entering
/// a block that is a handler, it is removed from the active set. If any JSX
/// instruction is found while active try blocks exist, an error is reported.
pub fn validate_no_jsx_in_try_statement(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut active_try_blocks: Vec<BlockId> = Vec::new();
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    for (_bid, block) in &func.body.blocks {
        // Remove this block from active try blocks if it is a handler.
        // This mirrors the upstream `retainWhere(activeTryBlocks, id => id !== block.id)`.
        active_try_blocks.retain(|id| *id != block.id);

        if !active_try_blocks.is_empty() {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. } => {
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message:
                                "Avoid constructing JSX within try/catch. \
                                 React does not immediately render components when JSX is rendered, \
                                 so any errors from this component will not be caught by the try/catch. \
                                 To catch errors in rendering a given component, wrap that component \
                                 in an error boundary. \
                                 (https://react.dev/reference/react/Component#catching-rendering-errors-with-an-error-boundary)"
                                    .to_string(),
                        });
                        break;
                    }
                    _ => {}
                }
            }
        }

        // If this block's terminal is a Try, push its handler onto the active set.
        if let Terminal::Try { handler, .. } = &block.terminal {
            active_try_blocks.push(*handler);
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "JSX found inside try/catch statement".to_string(),
            diagnostics,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_test_place(id: u32, name: Option<&str>) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: name.map(|n| IdentifierName::Named(n.to_string())),
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

    fn make_basic_block_with_terminal(
        id: u32,
        instructions: Vec<Instruction>,
        terminal: Terminal,
    ) -> (BlockId, BasicBlock) {
        let bid = BlockId(id);
        (
            bid,
            BasicBlock {
                kind: BlockKind::Block,
                id: bid,
                instructions,
                terminal,
                preds: HashSet::new(),
                phis: vec![],
            },
        )
    }

    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_test_place(0, None),
            context: vec![],
            body: HIR {
                entry: blocks.first().map(|(id, _)| *id).unwrap_or(BlockId(0)),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    fn make_jsx_instruction(instr_id: u32, lvalue_id: u32) -> Instruction {
        Instruction {
            id: InstructionId(instr_id),
            lvalue: make_test_place(lvalue_id, None),
            value: InstructionValue::JsxExpression {
                tag: JsxTag::BuiltinTag("div".to_string()),
                props: vec![],
                children: None,
                loc: SourceLocation::Generated,
                opening_loc: SourceLocation::Generated,
                closing_loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }
    }

    #[test]
    fn test_no_jsx_no_try_is_ok() {
        // A simple function with no JSX and no try -> should pass.
        let block = make_basic_block_with_terminal(
            0,
            vec![Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(100, None),
                value: InstructionValue::Primitive {
                    value: PrimitiveValue::Number(42.0),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            }],
            Terminal::Return {
                value: make_test_place(100, None),
                return_variant: ReturnVariant::Explicit,
                id: InstructionId(1),
                loc: SourceLocation::Generated,
            },
        );
        let func = make_hir_function(vec![block]);
        assert!(validate_no_jsx_in_try_statement(&func).is_ok());
    }

    #[test]
    fn test_jsx_outside_try_is_ok() {
        // JSX is in block 0 which is before the try; block 1 is the try block
        // with no JSX; block 2 is the handler; block 3 is fallthrough.
        let blocks = vec![
            make_basic_block_with_terminal(
                0,
                vec![make_jsx_instruction(0, 100)],
                Terminal::Try {
                    block: BlockId(1),
                    handler_binding: None,
                    handler: BlockId(2),
                    fallthrough: BlockId(3),
                    id: InstructionId(1),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                1,
                vec![], // no JSX in try body
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                2,
                vec![], // handler
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                3,
                vec![],
                Terminal::Return {
                    value: make_test_place(200, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(4),
                    loc: SourceLocation::Generated,
                },
            ),
        ];
        let func = make_hir_function(blocks);
        assert!(validate_no_jsx_in_try_statement(&func).is_ok());
    }

    #[test]
    fn test_jsx_inside_try_block_fails() {
        // Block 0: Try terminal -> block 1 (try body), handler at block 2, fallthrough block 3
        // Block 1: contains JSX instruction (inside the try body)
        // Block 2: handler
        // Block 3: fallthrough
        let blocks = vec![
            make_basic_block_with_terminal(
                0,
                vec![],
                Terminal::Try {
                    block: BlockId(1),
                    handler_binding: None,
                    handler: BlockId(2),
                    fallthrough: BlockId(3),
                    id: InstructionId(0),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                1,
                vec![make_jsx_instruction(1, 100)], // JSX inside try body
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                2,
                vec![],
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                3,
                vec![],
                Terminal::Return {
                    value: make_test_place(200, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(4),
                    loc: SourceLocation::Generated,
                },
            ),
        ];
        let func = make_hir_function(blocks);
        let result = validate_no_jsx_in_try_statement(&func);
        assert!(result.is_err());
        if let Err(CompilerError::Bail(bailout)) = result {
            assert!(bailout.diagnostics.iter().any(|d| {
                d.message
                    .contains("Avoid constructing JSX within try/catch")
            }));
        }
    }

    #[test]
    fn test_jsx_in_catch_without_outer_try_is_ok() {
        // Block 0: Try terminal -> block 1 (try body), handler at block 2, fallthrough block 3
        // Block 1: no JSX
        // Block 2: handler with JSX (this is inside catch, NOT inside a try, so it's fine)
        // Block 3: fallthrough
        //
        // The handler block (2) is pushed as an active try block. When we reach block 2,
        // it's removed from active_try_blocks (retain). So JSX in the handler is allowed.
        let blocks = vec![
            make_basic_block_with_terminal(
                0,
                vec![],
                Terminal::Try {
                    block: BlockId(1),
                    handler_binding: None,
                    handler: BlockId(2),
                    fallthrough: BlockId(3),
                    id: InstructionId(0),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                1,
                vec![],
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(1),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                2,
                vec![make_jsx_instruction(2, 100)], // JSX in catch handler
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                3,
                vec![],
                Terminal::Return {
                    value: make_test_place(200, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(4),
                    loc: SourceLocation::Generated,
                },
            ),
        ];
        let func = make_hir_function(blocks);
        assert!(validate_no_jsx_in_try_statement(&func).is_ok());
    }

    #[test]
    fn test_jsx_fragment_inside_try_fails() {
        let blocks = vec![
            make_basic_block_with_terminal(
                0,
                vec![],
                Terminal::Try {
                    block: BlockId(1),
                    handler_binding: None,
                    handler: BlockId(2),
                    fallthrough: BlockId(3),
                    id: InstructionId(0),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                1,
                vec![Instruction {
                    id: InstructionId(1),
                    lvalue: make_test_place(100, None),
                    value: InstructionValue::JsxFragment {
                        children: vec![],
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                }],
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                2,
                vec![],
                Terminal::Goto {
                    block: BlockId(3),
                    variant: GotoVariant::Break,
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
            ),
            make_basic_block_with_terminal(
                3,
                vec![],
                Terminal::Return {
                    value: make_test_place(200, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(4),
                    loc: SourceLocation::Generated,
                },
            ),
        ];
        let func = make_hir_function(blocks);
        assert!(validate_no_jsx_in_try_statement(&func).is_err());
    }
}
