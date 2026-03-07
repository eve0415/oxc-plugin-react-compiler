//! Prune hoisted context declarations from the reactive function tree.
//!
//! Port of `PruneHoistedContexts.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass:
//! 1. Removes `DeclareContext` instructions that have a hoisted lvalue kind
//!    (HoistedConst, HoistedLet, HoistedFunction) to preserve TDZ semantics.
//! 2. Rewrites `StoreContext` instructions whose lvalue is declared by an
//!    enclosing scope from `Let`/`Const` to `Reassign`, since the variable
//!    will already be pre-declared before the scope guard.
//! 3. Tracks function declarations that are hoisted and bails out if they
//!    are referenced before their definition (to avoid invalid accesses).

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;

/// Prune hoisted context declarations and rewrite store kinds.
///
/// Operates on the ReactiveFunction tree.
pub fn prune_hoisted_contexts(func: &mut ReactiveFunction) -> Result<(), CompilerError> {
    let debug = std::env::var("DEBUG_PRUNE_HOISTED_CONTEXTS").is_ok();
    if debug {
        eprintln!(
            "[PRUNE_HOISTED] begin fn={:?} body_len={}",
            func.name_hint,
            func.body.len()
        );
    }
    let mut state = VisitorState {
        active_scopes: Vec::new(),
        uninitialized: HashMap::new(),
        referenced_before_init: HashSet::new(),
        initialized: HashSet::new(),
    };
    transform_block(&mut func.body, &mut state)
}

/// Converts a hoisted lvalue kind to its non-hoisted equivalent.
/// Returns `None` for non-hoisted kinds (meaning: do not remove).
/// Returns `Some(kind)` for hoisted kinds.
fn convert_hoisted_lvalue_kind(kind: InstructionKind) -> Option<InstructionKind> {
    match kind {
        InstructionKind::HoistedLet => Some(InstructionKind::Let),
        InstructionKind::HoistedConst => Some(InstructionKind::Const),
        InstructionKind::HoistedFunction => Some(InstructionKind::Function),
        InstructionKind::Let
        | InstructionKind::Const
        | InstructionKind::Function
        | InstructionKind::Reassign
        | InstructionKind::Catch => None,
    }
}

#[derive(Debug)]
enum UninitializedEntry {
    UnknownKind,
    Func { definition: Option<Place> },
}

struct VisitorState {
    /// Stack of sets of declaration IDs declared by active scopes.
    active_scopes: Vec<HashSet<DeclarationId>>,
    /// Tracks declaration IDs that are declared by a scope but not yet initialized.
    uninitialized: HashMap<DeclarationId, UninitializedEntry>,
    /// Tracks declaration IDs referenced before initialization while in `unknown-kind` state.
    referenced_before_init: HashSet<DeclarationId>,
    /// Tracks declaration IDs that already received an initializing store.
    initialized: HashSet<DeclarationId>,
}

impl VisitorState {
    fn is_declared_by_scope(&self, id: &DeclarationId) -> bool {
        self.active_scopes.iter().any(|scope| scope.contains(id))
    }
}

fn store_value_captures_outer_scope(value: &Place, lvalue_decl: DeclarationId) -> bool {
    value.identifier.scope.as_ref().is_some_and(|scope| {
        scope
            .dependencies
            .iter()
            .any(|dep| dep.identifier.declaration_id != lvalue_decl)
    })
}

fn transform_block(
    block: &mut ReactiveBlock,
    state: &mut VisitorState,
) -> Result<(), CompilerError> {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(_) => {
                let should_remove = transform_instruction_in_place(&mut block[i], state)?;
                if should_remove {
                    block.remove(i);
                    // Don't increment i
                } else {
                    i += 1;
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(&mut term_stmt.terminal, state)?;
                i += 1;
            }
            ReactiveStatement::Scope(scope_block) => {
                // Push scope declarations onto active_scopes
                let scope_ids: HashSet<DeclarationId> = scope_block
                    .scope
                    .declarations
                    .values()
                    .map(|decl| decl.identifier.declaration_id)
                    .collect();

                // Mark all declarations as uninitialized
                for decl in scope_block.scope.declarations.values() {
                    state.uninitialized.insert(
                        decl.identifier.declaration_id,
                        UninitializedEntry::UnknownKind,
                    );
                }

                state.active_scopes.push(scope_ids.clone());
                transform_block(&mut scope_block.instructions, state)?;
                state.active_scopes.pop();

                // Clean up uninitialized tracking for this scope's declarations
                for decl in scope_block.scope.declarations.values() {
                    state.uninitialized.remove(&decl.identifier.declaration_id);
                    state
                        .referenced_before_init
                        .remove(&decl.identifier.declaration_id);
                }

                i += 1;
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                transform_block(&mut scope_block.instructions, state)?;
                i += 1;
            }
        }
    }
    Ok(())
}

/// Transform an instruction in place. Returns true if the instruction should be removed.
fn transform_instruction_in_place(
    stmt: &mut ReactiveStatement,
    state: &mut VisitorState,
) -> Result<bool, CompilerError> {
    let debug = std::env::var("DEBUG_PRUNE_HOISTED_CONTEXTS").is_ok();
    let instr = match stmt {
        ReactiveStatement::Instruction(instr) => instr,
        _ => return Ok(false),
    };

    if debug {
        match &instr.value {
            InstructionValue::DeclareContext { lvalue, .. }
            | InstructionValue::DeclareLocal { lvalue, .. } => {
                eprintln!(
                    "[PRUNE_HOISTED] decl kind={:?} id={} decl={} name={:?}",
                    lvalue.kind,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    lvalue.place.identifier.name
                );
            }
            InstructionValue::StoreContext { lvalue, value, .. }
            | InstructionValue::StoreLocal { lvalue, value, .. } => {
                eprintln!(
                    "[PRUNE_HOISTED] store kind={:?} id={} decl={} value_id={} value_decl={} name={:?}",
                    lvalue.kind,
                    lvalue.place.identifier.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    value.identifier.id.0,
                    value.identifier.declaration_id.0,
                    lvalue.place.identifier.name
                );
            }
            InstructionValue::LoadContext { place, .. }
            | InstructionValue::LoadLocal { place, .. } => {
                eprintln!(
                    "[PRUNE_HOISTED] load id={} decl={} name={:?}",
                    place.identifier.id.0, place.identifier.declaration_id.0, place.identifier.name
                );
            }
            _ => {}
        }
    }

    // Check for DeclareContext with hoisted kind -> remove
    if let InstructionValue::DeclareContext { lvalue, .. } = &instr.value {
        let maybe_non_hoisted = convert_hoisted_lvalue_kind(lvalue.kind);
        if let Some(non_hoisted_kind) = maybe_non_hoisted {
            // If it's a hoisted function declaration that's tracked as uninitialized,
            // update its entry to be a func entry
            if non_hoisted_kind == InstructionKind::Function
                && state
                    .uninitialized
                    .contains_key(&lvalue.place.identifier.declaration_id)
            {
                state.uninitialized.insert(
                    lvalue.place.identifier.declaration_id,
                    UninitializedEntry::Func { definition: None },
                );
            }
            return Ok(true); // Remove this instruction
        }
    }

    // Check for scope-local stores with non-Reassign kind -> maybe rewrite to Reassign
    // and/or detect hoisted function references used before definition.
    let store_captures_outer_scope = match &instr.value {
        InstructionValue::StoreContext { lvalue, value, .. }
        | InstructionValue::StoreLocal { lvalue, value, .. } => {
            lvalue.kind != InstructionKind::Reassign
                && store_value_captures_outer_scope(value, lvalue.place.identifier.declaration_id)
        }
        _ => false,
    };
    if let Some(lvalue) = match &mut instr.value {
        InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::StoreLocal { lvalue, .. } => Some(lvalue),
        _ => None,
    } && lvalue.kind != InstructionKind::Reassign
    {
        let lvalue_id = lvalue.place.identifier.declaration_id;
        let is_declared_by_scope = state.is_declared_by_scope(&lvalue_id);
        let is_function_typed = matches!(lvalue.place.identifier.type_, Type::Function { .. });

        if debug {
            eprintln!(
                "[PRUNE_HOISTED] store-check id={} declared_by_scope={} function_typed={} referenced_before_init={} initialized={} captures_outer_scope={} type={:?}",
                lvalue.place.identifier.id.0,
                is_declared_by_scope,
                is_function_typed,
                state.referenced_before_init.contains(&lvalue_id),
                state.initialized.contains(&lvalue_id),
                store_captures_outer_scope,
                lvalue.place.identifier.type_
            );
        }
        if is_function_typed
            && !state.initialized.contains(&lvalue_id)
            && state.referenced_before_init.contains(&lvalue_id)
            && store_captures_outer_scope
        {
            return Err(CompilerError::Bail(BailOut {
                reason: "[PruneHoistedContexts] Rewrite hoisted function references".to_string(),
                diagnostics: vec![CompilerDiagnostic {
                    severity: DiagnosticSeverity::Todo,
                    message: "[PruneHoistedContexts] Rewrite hoisted function references"
                        .to_string(),
                }],
            }));
        }

        if is_declared_by_scope {
            match lvalue.kind {
                InstructionKind::Let | InstructionKind::Const => {
                    lvalue.kind = InstructionKind::Reassign;
                }
                InstructionKind::Function => {
                    if let Some(maybe_hoisted_fn) = state.uninitialized.get_mut(&lvalue_id) {
                        if !matches!(
                            maybe_hoisted_fn,
                            UninitializedEntry::Func { .. } | UninitializedEntry::UnknownKind
                        ) {
                            return Err(CompilerError::Bail(BailOut {
                                reason: "[PruneHoistedContexts] Unexpected hoisted function"
                                    .to_string(),
                                diagnostics: vec![CompilerDiagnostic {
                                    severity: DiagnosticSeverity::Invariant,
                                    message: "[PruneHoistedContexts] Unexpected hoisted function"
                                        .to_string(),
                                }],
                            }));
                        }
                        if let UninitializedEntry::Func { definition } = maybe_hoisted_fn {
                            *definition = Some(lvalue.place.clone());
                        }
                        // References after this assignment are safe.
                        state.uninitialized.remove(&lvalue_id);
                    }
                }
                _ => {
                    return Err(CompilerError::Bail(BailOut {
                        reason: "[PruneHoistedContexts] Unexpected kind".to_string(),
                        diagnostics: vec![CompilerDiagnostic {
                            severity: DiagnosticSeverity::Todo,
                            message: format!(
                                "[PruneHoistedContexts] Unexpected kind ({:?})",
                                lvalue.kind
                            ),
                        }],
                    }));
                }
            }
        }
        if is_function_typed {
            state.uninitialized.remove(&lvalue_id);
            state.referenced_before_init.remove(&lvalue_id);
            state.initialized.insert(lvalue_id);
        }
    }

    // Visit all places in the instruction to check for hoisted function references
    visit_instruction_places(instr, state)?;

    Ok(false)
}

/// Visit all Place references in an instruction to check for references to
/// hoisted functions before their definition.
fn visit_instruction_places(
    instr: &ReactiveInstruction,
    state: &mut VisitorState,
) -> Result<(), CompilerError> {
    let debug = std::env::var("DEBUG_PRUNE_HOISTED_CONTEXTS").is_ok();
    let mut visit = |place: &Place| {
        let decl_id = place.identifier.declaration_id;
        if !state.initialized.contains(&decl_id) {
            state.referenced_before_init.insert(decl_id);
        }
        if let Some(entry) = state.uninitialized.get(&decl_id) {
            match entry {
                UninitializedEntry::Func { definition } => {
                    if definition
                        .as_ref()
                        .is_some_and(|def| def.identifier.id == place.identifier.id)
                    {
                        return Ok(());
                    }
                    if debug {
                        eprintln!(
                            "[PRUNE_HOISTED] bail ref id={} decl={} name={:?}",
                            place.identifier.id.0,
                            place.identifier.declaration_id.0,
                            place.identifier.name
                        );
                    }
                    return Err(CompilerError::Bail(BailOut {
                        reason: "[PruneHoistedContexts] Rewrite hoisted function references"
                            .to_string(),
                        diagnostics: vec![CompilerDiagnostic {
                            severity: DiagnosticSeverity::Todo,
                            message: "[PruneHoistedContexts] Rewrite hoisted function references"
                                .to_string(),
                        }],
                    }));
                }
                UninitializedEntry::UnknownKind => {
                    state.referenced_before_init.insert(decl_id);
                }
            }
        }
        Ok(())
    };

    // Visit operands
    match &instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            visit(place)?;
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            visit(value)?;
        }
        InstructionValue::Destructure { value, .. } => {
            visit(value)?;
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            visit(left)?;
            visit(right)?;
        }
        InstructionValue::UnaryExpression { value, .. } => {
            visit(value)?;
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            visit(callee)?;
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => visit(p)?,
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            visit(receiver)?;
            visit(property)?;
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => visit(p)?,
                }
            }
        }
        _ => {
            // For other instruction kinds, the place-level visit is less critical
            // since hoisted function references are primarily through loads/stores/calls
        }
    }
    Ok(())
}

fn transform_terminal(
    terminal: &mut ReactiveTerminal,
    state: &mut VisitorState,
) -> Result<(), CompilerError> {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(consequent, state)?;
            if let Some(alt) = alternate {
                transform_block(alt, state)?;
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(block, state)?;
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(loop_block, state)?;
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(init, state)?;
            if let Some(upd) = update {
                transform_block(upd, state)?;
            }
            transform_block(loop_block, state)?;
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(init, state)?;
            transform_block(loop_block, state)?;
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(init, state)?;
            transform_block(loop_block, state)?;
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(block, state)?;
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(block, state)?;
            transform_block(handler, state)?;
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
    Ok(())
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
            declarations: std::collections::HashMap::new(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_remove_hoisted_const_declare_context() {
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
                    value: InstructionValue::DeclareContext {
                        lvalue: LValue {
                            kind: InstructionKind::HoistedConst,
                            place: make_place(1, Some(IdentifierName::Named("x".to_string()))),
                        },
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
            directives: vec![],
        };

        prune_hoisted_contexts(&mut func);
        assert!(
            func.body.is_empty(),
            "Hoisted DeclareContext should be removed"
        );
    }

    #[test]
    fn test_keep_non_hoisted_declare_context() {
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
                    value: InstructionValue::DeclareContext {
                        lvalue: LValue {
                            kind: InstructionKind::Let,
                            place: make_place(1, Some(IdentifierName::Named("x".to_string()))),
                        },
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
            directives: vec![],
        };

        prune_hoisted_contexts(&mut func);
        assert_eq!(
            func.body.len(),
            1,
            "Non-hoisted DeclareContext should be kept"
        );
    }

    #[test]
    fn test_rewrite_store_context_to_reassign() {
        // Create a scope that declares x (id=1), then a StoreContext(Let) for x
        let mut scope = make_scope(1);
        scope.declarations.insert(
            IdentifierId(1),
            ScopeDeclaration {
                identifier: make_identifier(1, Some(IdentifierName::Named("x".to_string()))),
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
                        value: InstructionValue::StoreContext {
                            lvalue: LValue {
                                kind: InstructionKind::Let,
                                place: make_place(1, Some(IdentifierName::Named("x".to_string()))),
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

        prune_hoisted_contexts(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            if let ReactiveStatement::Instruction(instr) = &scope_block.instructions[0] {
                if let InstructionValue::StoreContext { lvalue, .. } = &instr.value {
                    assert_eq!(
                        lvalue.kind,
                        InstructionKind::Reassign,
                        "StoreContext Let should be rewritten to Reassign"
                    );
                } else {
                    panic!("Expected StoreContext instruction");
                }
            } else {
                panic!("Expected Instruction statement");
            }
        } else {
            panic!("Expected Scope statement");
        }
    }
}
