//! Propagate early returns in reactive scopes.
//!
//! Port of `PropagateEarlyReturns.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass ensures that reactive blocks honor the control flow behavior of the
//! original code including early return semantics. If a reactive scope early
//! returned during the previous execution and the inputs to that block have not
//! changed, then the code should early return (with the same value) again.
//!
//! For each top-level reactive scope that transitively contains an early return:
//!
//! - Label the scope
//! - Synthesize a new temporary (e.g. `t0`) and set it as a declaration of the scope.
//!   This represents the possibly-unset return value for that scope.
//! - Make the first instruction of the scope the declaration of that temporary,
//!   assigning a sentinel value (`Symbol.for('react.early_return_sentinel')`).
//! - Replace all `return` statements with:
//!   - An assignment of the temporary with the value being returned.
//!   - A `break` to the reactive scope's label.
//!
//! Finally, CodegenReactiveScope adds an if-check following the reactive scope:
//! if the early return temporary value is *not* the sentinel value, we early return
//! it. Otherwise, execution continues.

use crate::hir::builder::IdCounter;
use crate::hir::types::*;

/// The sentinel string used for early return detection.
/// Corresponds to `EARLY_RETURN_SENTINEL` in upstream `CodegenReactiveFunction.ts`.
pub const EARLY_RETURN_SENTINEL: &str = "react.early_return_sentinel";

/// Propagate early returns through reactive scopes in a ReactiveFunction.
///
/// Transforms `return` statements inside reactive scopes into
/// assign-and-break patterns, and annotates the outermost containing
/// scope with `earlyReturnValue` metadata for codegen.
pub fn propagate_early_returns(func: &mut ReactiveFunction) {
    let mut ids = IdCounter::new();
    // Initialize the counter past any existing IDs to avoid collisions.
    // We scan all block IDs and identifier IDs in the function and set
    // the counter above the maximum.
    init_id_counter(&func.body, &mut ids);

    let mut state = State {
        within_reactive_scope: false,
        early_return_value: None,
    };
    transform_block(&mut func.body, &mut state, &mut ids);
}

/// State threaded through the recursive walk.
struct State {
    /// Are we inside a reactive scope? Used to decide whether to transform
    /// return terminals and to annotate only the outermost scope.
    within_reactive_scope: bool,

    /// Bubbles early return info from inner returns up to the outermost
    /// reactive scope.
    early_return_value: Option<EarlyReturnValue>,
}

/// Walk all IDs in the reactive function to initialize the counter past existing values.
fn init_id_counter(block: &ReactiveBlock, ids: &mut IdCounter) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                ids.observe_identifier_id(instr.id);
                if let Some(lvalue) = &instr.lvalue {
                    ids.observe_identifier_id_from_ident(lvalue.identifier.id);
                }
                observe_instruction_value_ids(&instr.value, ids);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                if let Some(label) = &term_stmt.label {
                    ids.observe_block_id(label.id);
                }
                observe_terminal_ids(&term_stmt.terminal, ids);
            }
            ReactiveStatement::Scope(scope) => {
                init_id_counter(&scope.instructions, ids);
            }
            ReactiveStatement::PrunedScope(scope) => {
                init_id_counter(&scope.instructions, ids);
            }
        }
    }
}

/// Observe IDs from instruction values for counter initialization.
fn observe_instruction_value_ids(value: &InstructionValue, ids: &mut IdCounter) {
    // We only need to observe identifier IDs from places within instruction values.
    // This is a simplified scan -- we just need to ensure the counter is past all
    // existing IDs. We visit the common patterns that contain identifiers.
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            ids.observe_identifier_id_from_ident(place.identifier.id);
        }
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            ids.observe_identifier_id_from_ident(lvalue.place.identifier.id);
            ids.observe_identifier_id_from_ident(value.identifier.id);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            ids.observe_identifier_id_from_ident(lvalue.place.identifier.id);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            ids.observe_identifier_id_from_ident(callee.identifier.id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        ids.observe_identifier_id_from_ident(p.identifier.id);
                    }
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            ids.observe_identifier_id_from_ident(receiver.identifier.id);
            ids.observe_identifier_id_from_ident(property.identifier.id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        ids.observe_identifier_id_from_ident(p.identifier.id);
                    }
                }
            }
        }
        _ => {
            // For other instruction types, we rely on the instruction ID scan
            // and lvalue scan above to catch most IDs.
        }
    }
}

/// Observe IDs from terminals for counter initialization.
fn observe_terminal_ids(terminal: &ReactiveTerminal, ids: &mut IdCounter) {
    match terminal {
        ReactiveTerminal::Break { target, id, .. }
        | ReactiveTerminal::Continue { target, id, .. } => {
            ids.observe_block_id(*target);
            ids.observe_identifier_id(*id);
        }
        ReactiveTerminal::Return { value, id, .. } | ReactiveTerminal::Throw { value, id, .. } => {
            ids.observe_identifier_id_from_ident(value.identifier.id);
            ids.observe_identifier_id(*id);
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            id,
            ..
        } => {
            ids.observe_identifier_id(*id);
            init_id_counter(consequent, ids);
            if let Some(alt) = alternate {
                init_id_counter(alt, ids);
            }
        }
        ReactiveTerminal::Switch { cases, id, .. } => {
            ids.observe_identifier_id(*id);
            for case in cases {
                if let Some(block) = &case.block {
                    init_id_counter(block, ids);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, id, .. }
        | ReactiveTerminal::While { loop_block, id, .. } => {
            ids.observe_identifier_id(*id);
            init_id_counter(loop_block, ids);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            id,
            ..
        } => {
            ids.observe_identifier_id(*id);
            init_id_counter(init, ids);
            if let Some(upd) = update {
                init_id_counter(upd, ids);
            }
            init_id_counter(loop_block, ids);
        }
        ReactiveTerminal::ForOf {
            init,
            loop_block,
            id,
            ..
        } => {
            ids.observe_identifier_id(*id);
            init_id_counter(init, ids);
            init_id_counter(loop_block, ids);
        }
        ReactiveTerminal::ForIn {
            init,
            loop_block,
            id,
            ..
        } => {
            ids.observe_identifier_id(*id);
            init_id_counter(init, ids);
            init_id_counter(loop_block, ids);
        }
        ReactiveTerminal::Label { block, id, .. } => {
            ids.observe_identifier_id(*id);
            init_id_counter(block, ids);
        }
        ReactiveTerminal::Try {
            block, handler, id, ..
        } => {
            ids.observe_identifier_id(*id);
            init_id_counter(block, ids);
            init_id_counter(handler, ids);
        }
    }
}

/// Create a temporary Place with a fresh identifier ID.
fn create_temporary_place(ids: &mut IdCounter, loc: SourceLocation) -> Place {
    let id = ids.next_identifier_id();
    Place {
        identifier: Identifier {
            id,
            declaration_id: DeclarationId(id.0),
            name: None,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: make_type(),
            loc: loc.clone(),
        },
        effect: Effect::Unknown,
        reactive: false,
        loc: SourceLocation::Generated,
    }
}

/// Promote a temporary identifier to a named identifier.
/// Mirrors upstream `promoteTemporary` which sets name to `#t{declarationId}`.
fn promote_temporary(identifier: &mut Identifier) {
    debug_assert!(
        identifier.name.is_none(),
        "Expected a temporary (unnamed) identifier"
    );
    identifier.name = Some(IdentifierName::Promoted(format!(
        "#t{}",
        identifier.declaration_id.0
    )));
}

/// Transform a reactive block, processing statements in order.
/// When a terminal is transformed into multiple statements (replace-many),
/// the block is rebuilt.
fn transform_block(block: &mut ReactiveBlock, state: &mut State, ids: &mut IdCounter) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                // Instructions are not transformed by this pass
                i += 1;
            }
            ReactiveStatement::Scope(scope_block) => {
                visit_scope(scope_block, state, ids);
                i += 1;
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                // Traverse pruned scopes but don't treat them as reactive scopes
                transform_block(&mut scope_block.instructions, state, ids);
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let result = transform_terminal(term_stmt, state, ids);
                match result {
                    TransformResult::Keep => {
                        i += 1;
                    }
                    TransformResult::ReplaceMany(replacements) => {
                        // Remove the current statement and insert replacements
                        block.remove(i);
                        let count = replacements.len();
                        for (j, stmt) in replacements.into_iter().enumerate() {
                            block.insert(i + j, stmt);
                        }
                        i += count;
                    }
                }
            }
        }
    }
}

/// Result of transforming a terminal.
enum TransformResult {
    Keep,
    ReplaceMany(Vec<ReactiveStatement>),
}

/// Visit a reactive scope block. This is where the main logic lives:
/// - Traverse the scope's instructions with `within_reactive_scope = true`
/// - If an early return was found, annotate the outermost scope
fn visit_scope(
    scope_block: &mut ReactiveScopeBlock,
    parent_state: &mut State,
    ids: &mut IdCounter,
) {
    // Exit early if an earlier pass has already created an early return
    if scope_block.scope.early_return_value.is_some() {
        return;
    }

    let mut inner_state = State {
        within_reactive_scope: true,
        early_return_value: parent_state.early_return_value.clone(),
    };
    transform_block(&mut scope_block.instructions, &mut inner_state, ids);

    if let Some(early_return_value) = inner_state.early_return_value {
        if !parent_state.within_reactive_scope {
            // This is the outermost scope wrapping an early return.
            // Store the early return information on the scope.
            scope_block.scope.early_return_value = Some(early_return_value.clone());

            // Add the early return identifier to the scope's declarations.
            scope_block.scope.declarations.insert(
                early_return_value.value.id,
                ScopeDeclaration {
                    identifier: early_return_value.value.clone(),
                    scope: scope_block.scope.clone(),
                },
            );

            // Take the existing instructions out to wrap them in a label block.
            let original_instructions = std::mem::take(&mut scope_block.instructions);
            let loc = early_return_value.loc.clone();

            // Synthesize temporaries for the sentinel initialization:
            //   symbolTemp = LoadGlobal { name: "Symbol" }
            //   forTemp = PropertyLoad { object: symbolTemp, property: "for" }
            //   argTemp = Primitive { value: EARLY_RETURN_SENTINEL }
            //   sentinelTemp = MethodCall { receiver: symbolTemp, property: forTemp, args: [argTemp] }
            //   StoreLocal { lvalue: { kind: Let, place: earlyReturnValue.value }, value: sentinelTemp }
            let sentinel_temp = create_temporary_place(ids, loc.clone());
            let symbol_temp = create_temporary_place(ids, loc.clone());
            let for_temp = create_temporary_place(ids, loc.clone());
            let arg_temp = create_temporary_place(ids, loc.clone());

            // Instruction 1: symbolTemp = LoadGlobal { name: "Symbol" }
            let instr_load_symbol = ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                id: InstructionId(0),
                lvalue: Some(symbol_temp.clone()),
                value: InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global {
                        name: "Symbol".to_string(),
                    },
                    loc: loc.clone(),
                },
                loc: loc.clone(),
            }));

            // Instruction 2: forTemp = PropertyLoad { object: symbolTemp, property: "for" }
            let instr_property_load =
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(for_temp.clone()),
                    value: InstructionValue::PropertyLoad {
                        object: symbol_temp.clone(),
                        property: PropertyLiteral::String("for".to_string()),
                        optional: false,
                        loc: loc.clone(),
                    },
                    loc: loc.clone(),
                }));

            // Instruction 3: argTemp = Primitive { value: EARLY_RETURN_SENTINEL }
            let instr_primitive = ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                id: InstructionId(0),
                lvalue: Some(arg_temp.clone()),
                value: InstructionValue::Primitive {
                    value: PrimitiveValue::String(EARLY_RETURN_SENTINEL.to_string()),
                    loc: loc.clone(),
                },
                loc: loc.clone(),
            }));

            // Instruction 4: sentinelTemp = MethodCall { receiver: symbolTemp, property: forTemp, args: [argTemp] }
            let instr_method_call = ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                id: InstructionId(0),
                lvalue: Some(sentinel_temp.clone()),
                value: InstructionValue::MethodCall {
                    receiver: symbol_temp,
                    property: for_temp,
                    args: vec![Argument::Place(arg_temp)],
                    receiver_optional: false,
                    call_optional: false,
                    loc: loc.clone(),
                },
                loc: loc.clone(),
            }));

            // Instruction 5: StoreLocal { lvalue: earlyReturnValue.value, value: sentinelTemp }
            let instr_store_local = ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                id: InstructionId(0),
                lvalue: None,
                value: InstructionValue::StoreLocal {
                    lvalue: LValue {
                        place: Place {
                            identifier: early_return_value.value.clone(),
                            effect: Effect::ConditionallyMutate,
                            reactive: true,
                            loc: loc.clone(),
                        },
                        kind: InstructionKind::Let,
                    },
                    value: sentinel_temp,
                    loc: loc.clone(),
                },
                loc: loc.clone(),
            }));

            // Terminal: a labeled block wrapping the original instructions
            let label_terminal = ReactiveStatement::Terminal(ReactiveTerminalStatement {
                label: Some(ReactiveLabel {
                    id: early_return_value.label,
                    implicit: false,
                }),
                terminal: ReactiveTerminal::Label {
                    block: original_instructions,
                    id: InstructionId(0),
                    loc: SourceLocation::Generated,
                },
            });

            scope_block.instructions = vec![
                instr_load_symbol,
                instr_property_load,
                instr_primitive,
                instr_method_call,
                instr_store_local,
                label_terminal,
            ];
        } else {
            // Not the outermost scope. Pass the early return info up.
            parent_state.early_return_value = Some(early_return_value);
        }
    }
}

/// Transform a terminal statement. For `return` terminals inside a reactive
/// scope, replace with an assignment + break. For all other terminals,
/// recursively traverse their sub-blocks.
fn transform_terminal(
    stmt: &mut ReactiveTerminalStatement,
    state: &mut State,
    ids: &mut IdCounter,
) -> TransformResult {
    if state.within_reactive_scope
        && let ReactiveTerminal::Return { value, .. } = &stmt.terminal
    {
        let loc = value.loc.clone();

        // Get or create the early return value identifier
        let early_return_value = if let Some(existing) = &state.early_return_value {
            existing.clone()
        } else {
            let mut temp_place = create_temporary_place(ids, loc.clone());
            promote_temporary(&mut temp_place.identifier);
            EarlyReturnValue {
                label: ids.next_block_id(),
                loc: loc.clone(),
                value: temp_place.identifier,
            }
        };

        // Update state with this early return value (so it can be reused
        // for additional early returns in the same scope tree)
        state.early_return_value = Some(early_return_value.clone());

        // Clone the return value before we consume the terminal
        let return_value = value.clone();

        // Build replacement statements:
        // 1. StoreLocal (Reassign) of the return value into the early return identifier
        let store_instr = ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
            id: InstructionId(0),
            lvalue: None,
            value: InstructionValue::StoreLocal {
                lvalue: LValue {
                    place: Place {
                        identifier: early_return_value.value.clone(),
                        effect: Effect::Capture,
                        reactive: true,
                        loc: loc.clone(),
                    },
                    kind: InstructionKind::Reassign,
                },
                value: return_value,
                loc: loc.clone(),
            },
            loc: loc.clone(),
        }));

        // 2. Break to the scope's label
        let break_stmt = ReactiveStatement::Terminal(ReactiveTerminalStatement {
            label: None,
            terminal: ReactiveTerminal::Break {
                target: early_return_value.label,
                target_kind: ReactiveTerminalTargetKind::Labeled,
                id: InstructionId(0),
                loc: loc.clone(),
            },
        });

        return TransformResult::ReplaceMany(vec![store_instr, break_stmt]);
    }

    // For non-return terminals, recursively traverse sub-blocks
    traverse_terminal(&mut stmt.terminal, state, ids);
    TransformResult::Keep
}

/// Recursively traverse a terminal's sub-blocks.
fn traverse_terminal(terminal: &mut ReactiveTerminal, state: &mut State, ids: &mut IdCounter) {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {
            // No sub-blocks to traverse
        }
        ReactiveTerminal::Return { .. } | ReactiveTerminal::Throw { .. } => {
            // No sub-blocks to traverse (return is handled in transform_terminal)
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent, state, ids);
            if let Some(alt) = alternate {
                transform_block(alt, state, ids);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block, state, ids);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block, state, ids);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init, state, ids);
            if let Some(upd) = update {
                transform_block(upd, state, ids);
            }
            transform_block(loop_block, state, ids);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init, state, ids);
            transform_block(loop_block, state, ids);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init, state, ids);
            transform_block(loop_block, state, ids);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block, state, ids);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block, state, ids);
            transform_block(handler, state, ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_place(id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
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

    fn make_scope(scope_id: u32) -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(scope_id),
            range: MutableRange {
                start: InstructionId(0),
                end: InstructionId(100),
            },
            dependencies: vec![],
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    fn make_reactive_func(body: ReactiveBlock) -> ReactiveFunction {
        ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body,
            directives: vec![],
        }
    }

    #[test]
    fn test_no_early_return_is_noop() {
        // A function with no return inside a scope should be unchanged
        let mut func = make_reactive_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: make_scope(1),
                instructions: vec![ReactiveStatement::Instruction(Box::new(
                    ReactiveInstruction {
                        id: InstructionId(1),
                        lvalue: Some(make_place(1)),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Number(42.0),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                    },
                ))],
            }),
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return {
                    value: make_place(1),
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                label: None,
            }),
        ]);

        propagate_early_returns(&mut func);

        // The scope should not have earlyReturnValue set
        if let ReactiveStatement::Scope(scope) = &func.body[0] {
            assert!(scope.scope.early_return_value.is_none());
        } else {
            panic!("Expected Scope statement");
        }

        // The return should still be a return (not inside a scope)
        assert!(matches!(
            &func.body[1],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                ..
            })
        ));
    }

    #[test]
    fn test_early_return_inside_scope() {
        // A return inside a reactive scope should be transformed
        let mut func = make_reactive_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: make_scope(1),
            instructions: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(make_place(1)),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(42.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: make_place(1),
                        id: InstructionId(2),
                        loc: SourceLocation::Generated,
                    },
                    label: None,
                }),
            ],
        })]);

        propagate_early_returns(&mut func);

        // The scope should now have earlyReturnValue set
        if let ReactiveStatement::Scope(scope) = &func.body[0] {
            assert!(scope.scope.early_return_value.is_some());

            // The scope instructions should now be:
            // 1. LoadGlobal (Symbol)
            // 2. PropertyLoad (Symbol.for)
            // 3. Primitive (sentinel string)
            // 4. MethodCall (Symbol.for(sentinel))
            // 5. StoreLocal (let t = sentinel)
            // 6. Terminal(Label { ... })
            assert_eq!(scope.instructions.len(), 6);

            // Verify the label terminal wraps the original content
            if let ReactiveStatement::Terminal(term) = &scope.instructions[5] {
                assert!(term.label.is_some());
                assert!(!term.label.as_ref().unwrap().implicit);
                if let ReactiveTerminal::Label { block, .. } = &term.terminal {
                    // Inside the label block:
                    // - Original instruction
                    // - StoreLocal (Reassign) replacing the return
                    // - Break to label
                    assert_eq!(block.len(), 3);
                    assert!(matches!(&block[0], ReactiveStatement::Instruction(_)));
                    // The return should have been replaced with StoreLocal + Break
                    assert!(matches!(&block[1], ReactiveStatement::Instruction(_)));
                    assert!(matches!(
                        &block[2],
                        ReactiveStatement::Terminal(ReactiveTerminalStatement {
                            terminal: ReactiveTerminal::Break { .. },
                            ..
                        })
                    ));
                } else {
                    panic!("Expected Label terminal");
                }
            } else {
                panic!("Expected Terminal statement at position 5");
            }

            // Verify the early return value is in the scope's declarations
            let early = scope.scope.early_return_value.as_ref().unwrap();
            assert!(scope.scope.declarations.contains_key(&early.value.id));
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_early_return_in_if_inside_scope() {
        // A return inside an if inside a reactive scope
        let mut func = make_reactive_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: make_scope(1),
            instructions: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::If {
                    test: make_place(10),
                    consequent: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Return {
                            value: make_place(1),
                            id: InstructionId(3),
                            loc: SourceLocation::Generated,
                        },
                        label: None,
                    })],
                    alternate: None,
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                label: None,
            })],
        })]);

        propagate_early_returns(&mut func);

        if let ReactiveStatement::Scope(scope) = &func.body[0] {
            assert!(scope.scope.early_return_value.is_some());
            // Should have 6 items: 5 initialization instructions + 1 label terminal
            assert_eq!(scope.instructions.len(), 6);
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_already_has_early_return_value() {
        // If scope already has earlyReturnValue, the pass should skip it
        let mut scope = make_scope(1);
        scope.early_return_value = Some(EarlyReturnValue {
            value: make_place(99).identifier,
            loc: SourceLocation::Generated,
            label: BlockId(99),
        });

        let mut func = make_reactive_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope,
            instructions: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return {
                    value: make_place(1),
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                label: None,
            })],
        })]);

        propagate_early_returns(&mut func);

        // The scope should still have just one statement (the return, untransformed)
        if let ReactiveStatement::Scope(scope) = &func.body[0] {
            assert_eq!(scope.instructions.len(), 1);
            assert!(matches!(
                &scope.instructions[0],
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return { .. },
                    ..
                })
            ));
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_nested_scopes_early_return() {
        // Return inside a nested scope: only the outermost should get annotated
        let inner_scope = ReactiveScopeBlock {
            scope: make_scope(2),
            instructions: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return {
                    value: make_place(1),
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
                label: None,
            })],
        };

        let mut func = make_reactive_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: make_scope(1),
            instructions: vec![ReactiveStatement::Scope(inner_scope)],
        })]);

        propagate_early_returns(&mut func);

        if let ReactiveStatement::Scope(outer) = &func.body[0] {
            // The outermost scope should have earlyReturnValue
            assert!(outer.scope.early_return_value.is_some());

            // The inner scope should NOT have earlyReturnValue
            // (it bubbles up to the outermost)
            // The inner scope is now inside the label block (at index 5)
            if let ReactiveStatement::Terminal(term) = &outer.instructions[5] {
                if let ReactiveTerminal::Label { block, .. } = &term.terminal {
                    if let ReactiveStatement::Scope(inner) = &block[0] {
                        assert!(inner.scope.early_return_value.is_none());
                    }
                }
            }
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_multiple_returns_share_identifier() {
        // Multiple returns inside the same scope should share the same
        // early return identifier
        let mut func = make_reactive_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: make_scope(1),
            instructions: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::If {
                    test: make_place(10),
                    consequent: vec![ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::Return {
                            value: make_place(1),
                            id: InstructionId(3),
                            loc: SourceLocation::Generated,
                        },
                        label: None,
                    })],
                    alternate: Some(vec![ReactiveStatement::Terminal(
                        ReactiveTerminalStatement {
                            terminal: ReactiveTerminal::Return {
                                value: make_place(2),
                                id: InstructionId(4),
                                loc: SourceLocation::Generated,
                            },
                            label: None,
                        },
                    )]),
                    id: InstructionId(2),
                    loc: SourceLocation::Generated,
                },
                label: None,
            })],
        })]);

        propagate_early_returns(&mut func);

        if let ReactiveStatement::Scope(scope) = &func.body[0] {
            let early = scope.scope.early_return_value.as_ref().unwrap();

            // Inside the label block, find the if terminal
            if let ReactiveStatement::Terminal(term) = &scope.instructions[5] {
                if let ReactiveTerminal::Label { block, .. } = &term.terminal {
                    if let ReactiveStatement::Terminal(if_stmt) = &block[0] {
                        if let ReactiveTerminal::If {
                            consequent,
                            alternate,
                            ..
                        } = &if_stmt.terminal
                        {
                            // Both branches should use breaks to the same label
                            if let ReactiveStatement::Terminal(ReactiveTerminalStatement {
                                terminal: ReactiveTerminal::Break { target: t1, .. },
                                ..
                            }) = &consequent[1]
                            {
                                if let Some(alt) = alternate {
                                    if let ReactiveStatement::Terminal(
                                        ReactiveTerminalStatement {
                                            terminal: ReactiveTerminal::Break { target: t2, .. },
                                            ..
                                        },
                                    ) = &alt[1]
                                    {
                                        assert_eq!(*t1, early.label);
                                        assert_eq!(*t2, early.label);
                                        assert_eq!(*t1, *t2);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_return_outside_scope_not_transformed() {
        // Returns outside reactive scopes should not be transformed
        let mut func = make_reactive_func(vec![ReactiveStatement::Terminal(
            ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return {
                    value: make_place(1),
                    id: InstructionId(1),
                    loc: SourceLocation::Generated,
                },
                label: None,
            },
        )]);

        propagate_early_returns(&mut func);

        assert_eq!(func.body.len(), 1);
        assert!(matches!(
            &func.body[0],
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { .. },
                ..
            })
        ));
    }
}
