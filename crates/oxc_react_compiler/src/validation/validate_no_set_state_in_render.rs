//! Validates that setState calls don't occur directly during render.
//!
//! Port of `ValidateNoSetStateInRender.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This validates that the given function does not have an infinite update loop
//! caused by unconditionally calling setState during render. The validation
//! is conservative and cannot catch all cases of unconditional setState in
//! render, but avoids false positives.
//!
//! Examples of cases that are caught:
//!
//! ```javascript
//! // Direct call of setState:
//! const [state, setState] = useState(false);
//! setState(true);
//!
//! // Indirect via a function:
//! const [state, setState] = useState(false);
//! const setTrue = () => setState(true);
//! setTrue();
//! ```

use std::collections::HashSet;

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::compute_unconditional_blocks::compute_unconditional_blocks;
use crate::hir::types::*;
use crate::hir::visitors::for_each_instruction_operand;

/// Validates that setState is not called unconditionally during render.
///
/// Tracks useState return values (set functions) and checks if they are
/// called in unconditional blocks during render. Also tracks functions
/// that unconditionally call setState and propagates that information.
pub fn validate_no_set_state_in_render(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut unconditional_set_state_functions: HashSet<IdentifierId> = HashSet::new();
    validate_no_set_state_in_render_impl(func, &mut unconditional_set_state_functions)
}

fn validate_no_set_state_in_render_impl(
    func: &HIRFunction,
    unconditional_set_state_functions: &mut HashSet<IdentifierId>,
) -> Result<(), CompilerError> {
    let debug_set_state = std::env::var("DEBUG_SETSTATE_VALIDATION").is_ok();
    let unconditional_blocks = compute_unconditional_blocks(func);
    let mut active_manual_memo_id: Option<u32> = None;
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    if debug_set_state {
        let mut blocks: Vec<u32> = unconditional_blocks.iter().map(|b| b.0).collect();
        blocks.sort_unstable();
        eprintln!(
            "[SETSTATE_VALIDATION] fn={} unconditional_blocks={:?}",
            func.id.as_deref().unwrap_or("<anonymous>"),
            blocks
        );
    }

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if unconditional_set_state_functions.contains(&place.identifier.id) {
                        unconditional_set_state_functions.insert(instr.lvalue.identifier.id);
                        if debug_set_state {
                            eprintln!(
                                "[SETSTATE_VALIDATION] mark load lvalue_id={} from place_id={} place_ty={:?}",
                                instr.lvalue.identifier.id.0,
                                place.identifier.id.0,
                                place.identifier.type_
                            );
                        }
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if unconditional_set_state_functions.contains(&value.identifier.id) {
                        unconditional_set_state_functions.insert(lvalue.place.identifier.id);
                        unconditional_set_state_functions.insert(instr.lvalue.identifier.id);
                        if debug_set_state {
                            eprintln!(
                                "[SETSTATE_VALIDATION] mark store target_id={} instr_lvalue_id={} from value_id={} value_ty={:?}",
                                lvalue.place.identifier.id.0,
                                instr.lvalue.identifier.id.0,
                                value.identifier.id.0,
                                value.identifier.type_
                            );
                        }
                    }
                }
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    // Check if any operand of this function expression is a setState
                    let mut references_set_state = false;
                    for_each_instruction_operand(instr, |operand| {
                        if is_set_state_type(&operand.identifier)
                            || unconditional_set_state_functions.contains(&operand.identifier.id)
                        {
                            references_set_state = true;
                        }
                    });

                    if references_set_state {
                        // Check if the function body unconditionally calls setState
                        if validate_no_set_state_in_render_impl(
                            &lowered_func.func,
                            unconditional_set_state_functions,
                        )
                        .is_err()
                        {
                            // This function expression unconditionally calls setState
                            unconditional_set_state_functions.insert(instr.lvalue.identifier.id);
                        }
                    }
                }
                InstructionValue::StartMemoize { manual_memo_id, .. } => {
                    active_manual_memo_id = Some(*manual_memo_id);
                }
                InstructionValue::FinishMemoize { manual_memo_id, .. } => {
                    debug_assert!(
                        active_manual_memo_id == Some(*manual_memo_id),
                        "Expected FinishMemoize to align with previous StartMemoize instruction"
                    );
                    active_manual_memo_id = None;
                }
                InstructionValue::CallExpression { callee, .. } => {
                    if debug_set_state {
                        eprintln!(
                            "[SETSTATE_VALIDATION] call callee_id={} callee_ty={:?} in_unconditional={} tracked={}",
                            callee.identifier.id.0,
                            callee.identifier.type_,
                            unconditional_blocks.contains(&block.id),
                            unconditional_set_state_functions.contains(&callee.identifier.id)
                        );
                    }
                    if is_set_state_type(&callee.identifier)
                        || unconditional_set_state_functions.contains(&callee.identifier.id)
                    {
                        if active_manual_memo_id.is_some() {
                            diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidReact,
                                message:
                                    "Calling setState from useMemo may trigger an infinite loop. \
                                     Each time the memo callback is evaluated it will change state. \
                                     This can cause a memoization dependency to change, running the \
                                     memo function again and causing an infinite loop. Instead of \
                                     setting state in useMemo(), prefer deriving the value during render. \
                                     (https://react.dev/reference/react/useState)"
                                        .to_string(),
                            });
                        } else if unconditional_blocks.contains(&block.id) {
                            diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidReact,
                                message:
                                    "Calling setState during render may trigger an infinite loop. \
                                     Calling setState during render will trigger another render, \
                                     and can lead to infinite loops. \
                                     (https://react.dev/reference/react/useState)"
                                        .to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "setState called during render".to_string(),
            diagnostics,
        }))
    }
}

/// Checks if an identifier's type indicates it is a setState function.
///
/// In upstream this checks `id.type.kind === 'Function' && id.type.shapeId === 'BuiltInSetState'`.
/// We approximate this by checking the Function type's shape_id.
fn is_set_state_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Function { shape_id: Some(shape_id), .. } if shape_id == "BuiltInSetState"
    )
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

    #[test]
    fn test_no_set_state_calls_is_ok() {
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
        assert!(validate_no_set_state_in_render(&func).is_ok());
    }

    #[test]
    fn valid_set_state_in_callback() {
        let r = crate::test_utils::compile_to_result(
            "function Component() { const [x, setX] = useState(0); const onClick = () => setX(1); return <div onClick={onClick}>{x}</div>; }",
        );
        assert!(r.transformed);
    }

    #[test]
    fn set_state_in_render_bails() {
        let r = crate::test_utils::compile_to_result(
            "function Component() { const [x, setX] = useState(0); setX(1); return <div>{x}</div>; }",
        );
        assert!(!r.transformed);
    }

    #[test]
    fn conditional_set_state_does_not_bail() {
        // Conditional setState is not unconditional -- the validation only catches
        // direct unconditional calls during render, so this should compile fine.
        let r = crate::test_utils::compile_to_result(
            "function Component(props) { const [x, setX] = useState(0); if (props.cond) { setX(1); } return <div>{x}</div>; }",
        );
        assert!(r.transformed);
    }
}
