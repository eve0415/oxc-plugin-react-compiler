//! Validates proper use of useMemo/useCallback.
//!
//! Port of `ValidateUseMemo.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;

/// Validates that useMemo callbacks:
/// 1. Do not accept parameters
/// 2. Are not async or generator functions
///
/// Returns `Ok(())` if valid, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_use_memo(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();
    let mut use_memos: HashSet<IdentifierId> = HashSet::new();
    let mut react_ids: HashSet<IdentifierId> = HashSet::new();
    let mut functions: HashMap<IdentifierId, &LoweredFunction> = HashMap::new();
    // Track string literal values for MethodCall property resolution
    let mut id_string_values: HashMap<IdentifierId, String> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            // Track string literal values
            if let InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } = &instr.value
            {
                id_string_values.insert(instr.lvalue.identifier.id, s.clone());
            }
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name();
                    if name == "useMemo" {
                        use_memos.insert(instr.lvalue.identifier.id);
                    } else if name == "React" {
                        react_ids.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if react_ids.contains(&object.identifier.id)
                        && let PropertyLiteral::String(prop_name) = property
                        && prop_name == "useMemo"
                    {
                        use_memos.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    functions.insert(instr.lvalue.identifier.id, lowered_func);
                }
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    args,
                    ..
                } => {
                    let callee_id = property.identifier.id;
                    // Check both direct ID match and string value resolution
                    // for MethodCall where property is a Primitive::String temporary
                    let is_use_memo = use_memos.contains(&callee_id) || {
                        react_ids.contains(&receiver.identifier.id)
                            && id_string_values
                                .get(&callee_id)
                                .is_some_and(|s| s == "useMemo")
                    };
                    if !is_use_memo || args.is_empty() {
                        continue;
                    }
                    validate_use_memo_call(args, &functions, &mut diagnostics);
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    let callee_id = callee.identifier.id;
                    let is_use_memo = use_memos.contains(&callee_id);
                    if !is_use_memo || args.is_empty() {
                        continue;
                    }
                    validate_use_memo_call(args, &functions, &mut diagnostics);
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Invalid useMemo usage".to_string(),
            diagnostics,
        }))
    }
}

/// Checks if any terminal in the function has a non-void return.
fn has_non_void_return(func: &HIRFunction) -> bool {
    for (_bid, block) in &func.body.blocks {
        if let Terminal::Return { return_variant, .. } = &block.terminal
            && matches!(
                return_variant,
                ReturnVariant::Explicit | ReturnVariant::Implicit
            )
        {
            return true;
        }
    }
    false
}

/// Validates that useMemo callbacks return a value (not void).
///
/// Port of the void-return check from upstream `DropManualMemoization.ts` (line 447).
/// Gated behind `env.config.validateNoVoidUseMemo`.
///
/// Returns `Ok(())` if valid, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_no_void_use_memo(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();
    let mut use_memos: HashSet<IdentifierId> = HashSet::new();
    let mut react_use_memos: HashSet<IdentifierId> = HashSet::new();
    let mut react_ids: HashSet<IdentifierId> = HashSet::new();
    let mut functions: HashMap<IdentifierId, &LoweredFunction> = HashMap::new();
    let mut id_string_values: HashMap<IdentifierId, String> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } = &instr.value
            {
                id_string_values.insert(instr.lvalue.identifier.id, s.clone());
            }
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name();
                    if name == "useMemo" {
                        use_memos.insert(instr.lvalue.identifier.id);
                    } else if name == "React" {
                        react_ids.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if react_ids.contains(&object.identifier.id)
                        && let PropertyLiteral::String(prop_name) = property
                        && prop_name == "useMemo"
                    {
                        use_memos.insert(instr.lvalue.identifier.id);
                        react_use_memos.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    functions.insert(instr.lvalue.identifier.id, lowered_func);
                }
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    args,
                    ..
                } => {
                    let callee_id = property.identifier.id;
                    let is_method_use_memo = react_ids.contains(&receiver.identifier.id)
                        && id_string_values
                            .get(&callee_id)
                            .is_some_and(|s| s == "useMemo");
                    if !(use_memos.contains(&callee_id) || is_method_use_memo) || args.is_empty() {
                        continue;
                    }
                    let is_react = react_use_memos.contains(&callee_id) || is_method_use_memo;
                    validate_void_use_memo_call(args, &functions, is_react, &mut diagnostics);
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    let callee_id = callee.identifier.id;
                    if !use_memos.contains(&callee_id) || args.is_empty() {
                        continue;
                    }
                    let is_react = react_use_memos.contains(&callee_id);
                    validate_void_use_memo_call(args, &functions, is_react, &mut diagnostics);
                }
                _ => {}
            }
        }
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "useMemo callback doesn't return a value".to_string(),
            diagnostics,
        }))
    }
}

/// Check that the first argument of a useMemo call returns a value (not void).
fn validate_void_use_memo_call(
    args: &[Argument],
    functions: &HashMap<IdentifierId, &LoweredFunction>,
    is_react: bool,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    let first_arg = &args[0];
    let arg_id = match first_arg {
        Argument::Place(p) => p.identifier.id,
        Argument::Spread(_) => return,
    };

    let body = match functions.get(&arg_id) {
        Some(f) => f,
        None => return,
    };

    if !has_non_void_return(&body.func) {
        let prefix = if is_react { "React.useMemo" } else { "useMemo" };
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: format!(
                "useMemo() callbacks must return a value. This {} callback doesn't return \
                 a value. useMemo is for computing and caching values, not for arbitrary \
                 side effects",
                prefix
            ),
            category: None,
        });
    }
}

/// Check the first argument of a useMemo/useCallback call for parameter and
/// async/generator violations.
fn validate_use_memo_call(
    args: &[Argument],
    functions: &HashMap<IdentifierId, &LoweredFunction>,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    let first_arg = &args[0];
    let arg_id = match first_arg {
        Argument::Place(p) => p.identifier.id,
        Argument::Spread(_) => return,
    };

    let body = match functions.get(&arg_id) {
        Some(f) => f,
        None => return,
    };

    if !body.func.params.is_empty() {
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: "useMemo() callbacks may not accept parameters. \
                      useMemo() callbacks are called by React to cache calculations across re-renders. \
                      They should not take parameters. Instead, directly reference the props, state, \
                      or local variables needed for the computation"
                .to_string(),
            category: None,
        });
    }

    if body.func.async_ || body.func.generator {
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: "useMemo() callbacks may not be async or generator functions. \
                      useMemo() callbacks are called once and must synchronously return a value"
                .to_string(),
            category: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_basic_block(id: u32, instructions: Vec<Instruction>) -> (BlockId, BasicBlock) {
        let bid = BlockId(id);
        (
            bid,
            BasicBlock {
                kind: BlockKind::Block,
                id: bid,
                instructions,
                terminal: Terminal::Return {
                    value: make_test_place(999, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(999),
                    loc: SourceLocation::Generated,
                },
                preds: std::collections::HashSet::new(),
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

    #[test]
    fn test_no_use_memo_is_ok() {
        let instructions = vec![Instruction {
            id: InstructionId(0),
            lvalue: make_test_place(100, None),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Number(42.0),
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        }];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        assert!(validate_use_memo(&func).is_ok());
    }

    #[test]
    fn test_use_memo_with_params_fails() {
        // Set up: LoadGlobal("useMemo") -> id 1
        //         FunctionExpression with params -> id 2
        //         CallExpression(callee=1, args=[2])
        let inner_func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![Argument::Place(make_test_place(50, Some("arg")))],
            returns: make_test_place(51, None),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks: vec![make_basic_block(0, vec![])],
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(1, None),
                value: InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: "useMemo".to_string(),
                    },
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(2, None),
                value: InstructionValue::FunctionExpression {
                    name: None,
                    lowered_func: LoweredFunction { func: inner_func },
                    expr_type: FunctionExpressionType::ArrowFunctionExpression,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(2),
                lvalue: make_test_place(3, None),
                value: InstructionValue::CallExpression {
                    callee: make_test_place(1, None),
                    args: vec![Argument::Place(make_test_place(2, None))],
                    optional: false,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];

        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        let result = validate_use_memo(&func);
        assert!(result.is_err());
        if let Err(CompilerError::Bail(bailout)) = result {
            assert!(
                bailout.diagnostics[0]
                    .message
                    .contains("may not accept parameters")
            );
        }
    }
}
