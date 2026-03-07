//! Extract scope declarations from destructuring patterns.
//!
//! Port of `ExtractScopeDeclarationsFromDestructuring.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! When a destructuring statement contains a mix of variables that are declared
//! by an enclosing scope (and thus pre-declared as `let` before the scope guard)
//! and variables that are new within the scope, we cannot emit both as a single
//! destructuring statement. This pass rewrites such mixed destructurings by
//! replacing the pre-declared operands with fresh temporaries and emitting
//! separate `StoreLocal(Reassign)` instructions after the destructuring.

use std::collections::HashSet;

use crate::hir::types::*;

/// Extracts scope declarations from mixed destructuring patterns.
///
/// For each Destructure instruction where some pattern operands are declared
/// by an enclosing reactive scope and some are not, this pass:
/// 1. Replaces the scope-declared operands in the destructuring pattern with
///    fresh temporaries.
/// 2. Emits separate `StoreLocal { kind: Reassign }` instructions to assign
///    from the temporaries to the original scope-declared variables.
pub fn extract_scope_destructuring(func: &mut ReactiveFunction) {
    let mut declared: HashSet<DeclarationId> = HashSet::new();

    // Seed declared set from function params
    for param in &func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        declared.insert(place.identifier.declaration_id);
    }

    transform_block(&mut func.body, &mut declared);
}

fn transform_block(block: &mut ReactiveBlock, declared: &mut HashSet<DeclarationId>) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                // Check if it's a Destructure that needs transformation
                let needs_transform = if let ReactiveStatement::Instruction(instr) = &block[i] {
                    matches!(&instr.value, InstructionValue::Destructure { .. })
                        && should_transform_destructure(instr, declared)
                } else {
                    false
                };

                if needs_transform {
                    let stmt = block.remove(i);
                    if let ReactiveStatement::Instruction(instr) = stmt {
                        let new_stmts = do_transform_destructure(*instr, declared);
                        let count = new_stmts.len();
                        for (j, new_stmt) in new_stmts.into_iter().enumerate() {
                            block.insert(i + j, ReactiveStatement::Instruction(Box::new(new_stmt)));
                        }
                        i += count;
                    }
                } else {
                    i += 1;
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(&mut term_stmt.terminal, declared);
                i += 1;
            }
            ReactiveStatement::Scope(scope_block) => {
                // Add scope declarations to the declared set
                for decl in scope_block.scope.declarations.values() {
                    declared.insert(decl.identifier.declaration_id);
                }
                transform_block(&mut scope_block.instructions, declared);
                i += 1;
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                transform_block(&mut scope_block.instructions, declared);
                i += 1;
            }
        }
    }
}

fn transform_terminal(terminal: &mut ReactiveTerminal, declared: &mut HashSet<DeclarationId>) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent, declared);
            if let Some(alt) = alternate {
                transform_block(alt, declared);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block, declared);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block, declared);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init, declared);
            if let Some(upd) = update {
                transform_block(upd, declared);
            }
            transform_block(loop_block, declared);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init, declared);
            transform_block(loop_block, declared);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init, declared);
            transform_block(loop_block, declared);
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block, declared);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block, declared);
            transform_block(handler, declared);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

/// Check whether a Destructure instruction needs transformation.
///
/// Returns true if the destructuring pattern contains a mix of:
/// - Operands whose declaration is already in the `declared` set (reassigned)
/// - Operands whose declaration is NOT in the `declared` set (new declarations)
fn should_transform_destructure(
    instr: &ReactiveInstruction,
    declared: &HashSet<DeclarationId>,
) -> bool {
    if let InstructionValue::Destructure { lvalue, .. } = &instr.value {
        let mut has_reassigned = false;
        let mut has_declaration = false;
        for place in each_pattern_operand(&lvalue.pattern) {
            let is_declared = declared.contains(&place.identifier.declaration_id);
            if is_declared {
                has_reassigned = true;
            } else {
                has_declaration = true;
            }
        }
        has_reassigned && has_declaration
    } else {
        false
    }
}

/// Transform a Destructure instruction that has a mix of reassigned and new-declaration
/// operands. Returns the original (modified) destructure instruction followed by
/// StoreLocal(Reassign) instructions for each replaced operand.
fn do_transform_destructure(
    mut instr: ReactiveInstruction,
    declared: &HashSet<DeclarationId>,
) -> Vec<ReactiveInstruction> {
    // Collect which identifier IDs need to be reassigned
    let reassigned: HashSet<IdentifierId> =
        if let InstructionValue::Destructure { lvalue, .. } = &instr.value {
            each_pattern_operand(&lvalue.pattern)
                .into_iter()
                .filter(|place| declared.contains(&place.identifier.declaration_id))
                .map(|place| place.identifier.id)
                .collect()
        } else {
            return vec![instr];
        };

    if reassigned.is_empty() {
        return vec![instr];
    }

    // Replace reassigned operands with temporaries in the pattern
    let mut renamed: Vec<(Place, Place)> = Vec::new(); // (original, temporary)
    let destructure_loc = if let InstructionValue::Destructure { loc, .. } = &instr.value {
        loc.clone()
    } else {
        SourceLocation::Generated
    };

    if let InstructionValue::Destructure { lvalue, .. } = &mut instr.value {
        map_pattern_operands(&mut lvalue.pattern, |place| {
            if !reassigned.contains(&place.identifier.id) {
                return place;
            }
            let original = place.clone();
            let temporary = clone_place_to_temporary(&place);
            renamed.push((original, temporary.clone()));
            temporary
        });
    }

    // Build the result: the modified destructure instruction, then StoreLocal(Reassign) for each
    let mut instructions: Vec<ReactiveInstruction> = Vec::with_capacity(1 + renamed.len());
    let instr_id = instr.id;
    let instr_loc = instr.loc.clone();
    instructions.push(instr);

    for (original, temporary) in renamed {
        instructions.push(ReactiveInstruction {
            id: instr_id,
            lvalue: None,
            value: InstructionValue::StoreLocal {
                lvalue: LValue {
                    kind: InstructionKind::Reassign,
                    place: original,
                },
                value: temporary,
                loc: destructure_loc.clone(),
            },
            loc: instr_loc.clone(),
        });
    }

    instructions
}

/// Iterate over all Place operands of a destructuring pattern.
fn each_pattern_operand(pattern: &Pattern) -> Vec<&Place> {
    let mut result = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        result.push(place);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        result.push(&p.place);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        result.push(place);
                    }
                }
            }
        }
    }
    result
}

/// Apply a mapping function to each Place operand in a destructuring pattern.
fn map_pattern_operands<F>(pattern: &mut Pattern, mut f: F)
where
    F: FnMut(Place) -> Place,
{
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(place) => {
                        let taken = std::mem::replace(place, dummy_place());
                        *place = f(taken);
                    }
                    ArrayElement::Spread(place) => {
                        let taken = std::mem::replace(place, dummy_place());
                        *place = f(taken);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        let taken = std::mem::replace(&mut p.place, dummy_place());
                        p.place = f(taken);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        let taken = std::mem::replace(place, dummy_place());
                        *place = f(taken);
                    }
                }
            }
        }
    }
}

/// Create a new temporary Place cloned from the given place.
/// Corresponds to upstream `clonePlaceToTemporary` + `promoteTemporary`.
fn clone_place_to_temporary(place: &Place) -> Place {
    static NEXT_TEMP_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(900_000);
    let temp_id = NEXT_TEMP_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    Place {
        identifier: Identifier {
            id: IdentifierId(temp_id),
            declaration_id: DeclarationId(temp_id),
            name: Some(IdentifierName::Promoted(format!("#t{}", temp_id))),
            mutable_range: MutableRange::default(),
            scope: None,
            type_: place.identifier.type_.clone(),
            loc: place.loc.clone(),
        },
        effect: place.effect,
        reactive: place.reactive,
        loc: place.loc.clone(),
    }
}

/// Create a dummy place for use with `std::mem::replace`.
fn dummy_place() -> Place {
    Place {
        identifier: Identifier {
            id: IdentifierId(0),
            declaration_id: DeclarationId(0),
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

    fn make_place(id: u32, name: Option<IdentifierName>) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_scope(id: u32) -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(id),
            range: MutableRange::default(),
            dependencies: vec![],
            declarations: indexmap::IndexMap::new(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_no_transform_when_all_new_declarations() {
        // Destructure where none of the operands are already declared
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
                    value: InstructionValue::Destructure {
                        lvalue: LValuePattern {
                            pattern: Pattern::Object(ObjectPattern {
                                properties: vec![ObjectPropertyOrSpread::Property(
                                    ObjectProperty {
                                        key: ObjectPropertyKey::String("x".to_string()),
                                        type_: ObjectPropertyType::Property,
                                        place: make_place(
                                            1,
                                            Some(IdentifierName::Named("x".to_string())),
                                        ),
                                    },
                                )],
                            }),
                            kind: InstructionKind::Const,
                        },
                        value: make_place(10, None),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
            directives: vec![],
        };

        extract_scope_destructuring(&mut func);
        // Should remain a single instruction (no transformation)
        assert_eq!(func.body.len(), 1);
        assert!(matches!(&func.body[0], ReactiveStatement::Instruction(_)));
    }

    #[test]
    fn test_transform_mixed_destructure() {
        // Create a scope that declares `rest` (decl_id=2), then a destructure
        // that has both a new variable `x` (decl_id=1) and the already-declared `rest`.
        let mut scope = make_scope(1);
        scope.declarations.insert(
            IdentifierId(2),
            ScopeDeclaration {
                identifier: make_identifier(2, Some(IdentifierName::Named("rest".to_string()))),
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
                instructions: vec![ReactiveStatement::Instruction(Box::new(
                    ReactiveInstruction {
                        id: InstructionId(1),
                        lvalue: None,
                        value: InstructionValue::Destructure {
                            lvalue: LValuePattern {
                                pattern: Pattern::Object(ObjectPattern {
                                    properties: vec![
                                        ObjectPropertyOrSpread::Property(ObjectProperty {
                                            key: ObjectPropertyKey::String("x".to_string()),
                                            type_: ObjectPropertyType::Property,
                                            place: make_place(
                                                1,
                                                Some(IdentifierName::Named("x".to_string())),
                                            ),
                                        }),
                                        ObjectPropertyOrSpread::Spread(make_place(
                                            2,
                                            Some(IdentifierName::Named("rest".to_string())),
                                        )),
                                    ],
                                }),
                                kind: InstructionKind::Const,
                            },
                            value: make_place(10, None),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                    },
                ))],
            })],
            directives: vec![],
        };

        extract_scope_destructuring(&mut func);

        // The scope should now contain 2 instructions:
        // 1. The modified Destructure (with `rest` replaced by a temporary)
        // 2. A StoreLocal(Reassign) from the temporary back to `rest`
        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            assert_eq!(
                scope_block.instructions.len(),
                2,
                "Should have destructure + reassignment"
            );

            // First instruction is the destructure
            assert!(matches!(
                &scope_block.instructions[0],
                ReactiveStatement::Instruction(instr)
                    if matches!(instr.as_ref(), ReactiveInstruction {
                        value: InstructionValue::Destructure { .. },
                        ..
                    })
            ));

            // Second instruction is the StoreLocal Reassign
            if let ReactiveStatement::Instruction(store_instr) = &scope_block.instructions[1] {
                if let InstructionValue::StoreLocal { lvalue, .. } = &store_instr.value {
                    assert_eq!(lvalue.kind, InstructionKind::Reassign);
                    assert_eq!(
                        lvalue.place.identifier.name.as_ref().unwrap().value(),
                        "rest"
                    );
                } else {
                    panic!("Expected StoreLocal instruction");
                }
            } else {
                panic!("Expected Instruction statement");
            }
        } else {
            panic!("Expected Scope statement");
        }
    }
}
