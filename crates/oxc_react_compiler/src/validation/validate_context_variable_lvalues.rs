//! Validates that context variables are only used with StoreContext/LoadContext,
//! not StoreLocal/LoadLocal.
//!
//! Port of `ValidateContextVariableLValues.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::HashMap;

use crate::error::{
    BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory, extract_span,
};
use crate::hir::types::*;

/// The kind of reference observed for a given identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    Local,
    Context,
    Destructure,
}

/// Tracks the observed reference kind for each identifier.
struct IdentifierKinds {
    map: HashMap<IdentifierId, (RefKind, IdentifierId)>,
    /// Catch parameter bindings by name, used to detect the upstream-matching
    /// inconsistency where catch params get StoreLocal but LoadContext when
    /// captured by closures. Scoped per-function via save/restore in recursion.
    catch_like_by_name: HashMap<String, Place>,
}

impl IdentifierKinds {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            catch_like_by_name: HashMap::new(),
        }
    }
}

/// Validates that all store/load references to a given named identifier align
/// with the "kind" of that variable (normal variable or context variable).
/// For example, a context variable may not be loaded/stored with regular
/// StoreLocal/LoadLocal/Destructure instructions.
pub fn validate_context_variable_lvalues(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut identifier_kinds = IdentifierKinds::new();
    validate_impl(func, &mut identifier_kinds)
}

fn validate_impl(
    func: &HIRFunction,
    identifier_kinds: &mut IdentifierKinds,
) -> Result<(), CompilerError> {
    let debug = std::env::var("DEBUG_CONTEXT_LVALUES").is_ok();
    let handler_blocks: std::collections::HashSet<BlockId> = func
        .body
        .blocks
        .iter()
        .filter_map(|(_, block)| match &block.terminal {
            Terminal::Try { handler, .. } => Some(*handler),
            _ => None,
        })
        .collect();
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] context place id={} decl={} name={:?}",
                            lvalue.place.identifier.id.0,
                            lvalue.place.identifier.declaration_id.0,
                            lvalue.place.identifier.name
                        );
                    }
                    visit(identifier_kinds, &lvalue.place, RefKind::Context)?;
                }
                InstructionValue::LoadContext { place, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] context place id={} decl={} name={:?}",
                            place.identifier.id.0,
                            place.identifier.declaration_id.0,
                            place.identifier.name
                        );
                    }
                    visit(identifier_kinds, place, RefKind::Context)?;
                }
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] local place id={} decl={} name={:?}",
                            lvalue.place.identifier.id.0,
                            lvalue.place.identifier.declaration_id.0,
                            lvalue.place.identifier.name
                        );
                    }
                    visit(identifier_kinds, &lvalue.place, RefKind::Local)?;
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] local place id={} decl={} name={:?}",
                            lvalue.place.identifier.id.0,
                            lvalue.place.identifier.declaration_id.0,
                            lvalue.place.identifier.name
                        );
                    }
                    if handler_blocks.contains(&block.id)
                        && matches!(&value.identifier.name, Some(IdentifierName::Promoted(_)))
                        && let Some(name) = lvalue.place.identifier.name.as_ref()
                    {
                        identifier_kinds
                            .catch_like_by_name
                            .insert(name.value().to_string(), lvalue.place.clone());
                        if debug {
                            eprintln!(
                                "[DEBUG_CONTEXT_LVALUES] register-catch-name name={} id={} decl={}",
                                name.value(),
                                lvalue.place.identifier.id.0,
                                lvalue.place.identifier.declaration_id.0
                            );
                        }
                    }
                    visit(identifier_kinds, &lvalue.place, RefKind::Local)?;
                }
                InstructionValue::LoadLocal { place, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] local place id={} decl={} name={:?}",
                            place.identifier.id.0,
                            place.identifier.declaration_id.0,
                            place.identifier.name
                        );
                    }
                    visit(identifier_kinds, place, RefKind::Local)?;
                }
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name },
                    loc,
                } => {
                    if let Some(captured_place) =
                        identifier_kinds.catch_like_by_name.get(name).cloned()
                    {
                        let mut synthetic_place = captured_place;
                        synthetic_place.loc = loc.clone();
                        if debug {
                            eprintln!(
                                "[DEBUG_CONTEXT_LVALUES] load-global-as-context(catch) name={} id={} decl={}",
                                name,
                                synthetic_place.identifier.id.0,
                                synthetic_place.identifier.declaration_id.0
                            );
                        }
                        visit(identifier_kinds, &synthetic_place, RefKind::Context)?;
                    }
                }
                InstructionValue::StoreGlobal { name, loc, .. } => {
                    if let Some(captured_place) =
                        identifier_kinds.catch_like_by_name.get(name).cloned()
                    {
                        let mut synthetic_place = captured_place;
                        synthetic_place.loc = loc.clone();
                        if debug {
                            eprintln!(
                                "[DEBUG_CONTEXT_LVALUES] store-global-as-context(catch) name={} id={} decl={}",
                                name,
                                synthetic_place.identifier.id.0,
                                synthetic_place.identifier.declaration_id.0
                            );
                        }
                        visit(identifier_kinds, &synthetic_place, RefKind::Context)?;
                    }
                }
                InstructionValue::PostfixUpdate { lvalue, .. }
                | InstructionValue::PrefixUpdate { lvalue, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] local-update place id={} decl={} name={:?}",
                            lvalue.identifier.id.0,
                            lvalue.identifier.declaration_id.0,
                            lvalue.identifier.name
                        );
                    }
                    visit(identifier_kinds, lvalue, RefKind::Local)?;
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    for place in each_pattern_operand(&lvalue.pattern) {
                        if debug {
                            eprintln!(
                                "[DEBUG_CONTEXT_LVALUES] destructure place id={} decl={} name={:?}",
                                place.identifier.id.0,
                                place.identifier.declaration_id.0,
                                place.identifier.name
                            );
                        }
                        visit(identifier_kinds, place, RefKind::Destructure)?;
                    }
                }
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    if debug {
                        eprintln!(
                            "[DEBUG_CONTEXT_LVALUES] recurse lowered_func context_len={} blocks={}",
                            lowered_func.func.context.len(),
                            lowered_func.func.body.blocks.len()
                        );
                    }
                    // Save catch_like_by_name so that catch names registered in a
                    // sibling function don't leak across function boundaries. This
                    // prevents false positives when an unrelated scope has a local
                    // variable with the same name as a catch parameter.
                    let saved_catch_names = identifier_kinds.catch_like_by_name.clone();
                    validate_impl(&lowered_func.func, identifier_kinds)?;
                    identifier_kinds.catch_like_by_name = saved_catch_names;
                }
                _ => {
                    // For any other instruction that has lvalues, the upstream
                    // throws a todo. We check whether the instruction produces
                    // additional lvalues beyond the outer instruction lvalue
                    // and bail if so.
                    if has_instruction_value_lvalue(&instr.value) {
                        return Err(CompilerError::Bail(BailOut {
                            reason: format!(
                                "ValidateContextVariableLValues: unhandled instruction variant: {:?}",
                                std::mem::discriminant(&instr.value)
                            ),
                            diagnostics: vec![CompilerDiagnostic {
                                severity: DiagnosticSeverity::Todo,
                                message: "Handle lvalues for this instruction kind".to_string(),
                                category: ErrorCategory::Immutability,
                                span: extract_span(&instr.loc),
                                ..Default::default()
                            }],
                        }));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Visit a place and check that its reference kind is consistent with
/// previous observations.
fn visit(
    identifiers: &mut IdentifierKinds,
    place: &Place,
    kind: RefKind,
) -> Result<(), CompilerError> {
    let id = place.identifier.id;
    if let Some((prev_kind, _prev_id)) = identifiers.map.get(&id).copied() {
        let was_context = prev_kind == RefKind::Context;
        let is_context = kind == RefKind::Context;
        if was_context != is_context {
            if prev_kind == RefKind::Destructure || kind == RefKind::Destructure {
                return Err(CompilerError::Bail(BailOut {
                    reason: "Support destructuring of context variables".to_string(),
                    diagnostics: vec![CompilerDiagnostic {
                        severity: DiagnosticSeverity::Todo,
                        message: "Support destructuring of context variables".to_string(),
                        category: ErrorCategory::Immutability,
                        span: extract_span(&place.loc),
                        ..Default::default()
                    }],
                }));
            }

            let place_name = place
                .identifier
                .name
                .as_ref()
                .map_or("<unnamed>", |n| n.value());
            return Err(CompilerError::Bail(BailOut {
                reason: format!(
                    "Expected all references to a variable to be consistently local or context references. \
                     Identifier {} is referenced as a {:?} variable, but was previously referenced as a {:?} variable",
                    place_name, kind, prev_kind
                ),
                diagnostics: vec![CompilerDiagnostic {
                    severity: DiagnosticSeverity::Invariant,
                    message: format!("this is {:?}", prev_kind),
                    category: ErrorCategory::Immutability,
                    span: extract_span(&place.loc),
                    ..Default::default()
                }],
            }));
        }
    }
    identifiers.map.insert(id, (kind, id));
    Ok(())
}

/// Collect all places from a destructuring pattern.
fn each_pattern_operand(pattern: &Pattern) -> Vec<&Place> {
    let mut places = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => places.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => places.push(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => places.push(p),
                }
            }
        }
    }
    places
}

/// Check if an instruction value has additional lvalues beyond the instruction's
/// outer lvalue (i.e., it matches the `eachInstructionValueLValue` upstream iterator).
/// Returns `true` only for unhandled instruction kinds that define inner lvalues.
fn has_instruction_value_lvalue(_value: &InstructionValue) -> bool {
    // The upstream `eachInstructionValueLValue` yields inner lvalues for:
    // - StoreLocal/StoreContext/DeclareLocal/DeclareContext -> lvalue.place
    // - Destructure -> pattern operands
    // - PrefixUpdate/PostfixUpdate -> lvalue
    // All of these are already handled in the main match above.
    // Any other instruction kind does not produce inner lvalues, so this
    // should always return false for unhandled variants.
    false
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

    fn make_lvalue(id: u32, name: Option<&str>) -> LValue {
        LValue {
            place: make_test_place(id, name),
            kind: InstructionKind::Let,
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
    fn test_consistent_local_references() {
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(100, None),
                value: InstructionValue::DeclareLocal {
                    lvalue: make_lvalue(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(101, None),
                value: InstructionValue::LoadLocal {
                    place: make_test_place(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        assert!(validate_context_variable_lvalues(&func).is_ok());
    }

    #[test]
    fn test_consistent_context_references() {
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(100, None),
                value: InstructionValue::DeclareContext {
                    lvalue: make_lvalue(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(101, None),
                value: InstructionValue::LoadContext {
                    place: make_test_place(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        assert!(validate_context_variable_lvalues(&func).is_ok());
    }

    #[test]
    fn test_mixed_local_and_context_fails() {
        let instructions = vec![
            Instruction {
                id: InstructionId(0),
                lvalue: make_test_place(100, None),
                value: InstructionValue::DeclareLocal {
                    lvalue: make_lvalue(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
            Instruction {
                id: InstructionId(1),
                lvalue: make_test_place(101, None),
                value: InstructionValue::LoadContext {
                    place: make_test_place(1, Some("x")),
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
                effects: None,
            },
        ];
        let func = make_hir_function(vec![make_basic_block(0, instructions)]);
        assert!(validate_context_variable_lvalues(&func).is_err());
    }
}
