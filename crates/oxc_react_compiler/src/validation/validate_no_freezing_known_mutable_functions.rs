//! Validates that functions with known mutations cannot be passed where a frozen value is expected.
//!
//! Port of `ValidateNoFreezingKnownMutableFunctions.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Because `onClick` function mutates `cache` when called, `onClick` is equivalent to a mutable
//! variable. But unlike other mutable values like an array, the receiver of the function has
//! no way to avoid mutation -- for example, a function can receive an array and choose not to mutate
//! it, but there's no way to know that a function is mutable and avoid calling it.
//!
//! This pass detects functions with *known* mutations (Store or Mutate, not ConditionallyMutate)
//! that are passed where a frozen value is expected and rejects them.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::*;
use crate::hir::visitors::{for_each_instruction_operand, for_each_terminal_operand};
use crate::inference::aliasing_effects::AliasingEffect;

/// A tracked mutation effect -- stores the Place being mutated so we can report it.
#[derive(Clone)]
struct TrackedMutation {
    value: Place,
}

fn push_mutable_function_error(
    effect: &TrackedMutation,
    loc: &SourceLocation,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    let variable = match &effect.value.identifier.name {
        Some(IdentifierName::Named(name)) => format!("`{}`", name),
        _ => "a local variable".to_string(),
    };
    let _ = loc;
    diagnostics.push(CompilerDiagnostic {
        severity: DiagnosticSeverity::InvalidReact,
        message: format!(
            "Cannot modify local variables after render completes. \
                 This argument is a function which may reassign or mutate {} after render, \
                 which can cause inconsistent behavior on subsequent renders. \
                 Consider using state instead",
            variable
        ),
        category: Some(ErrorCategory::Immutability),
    });
}

/// Returns true if the type is a ref or ref-like mutable type (e.g. BuiltInUseRefId or ReanimatedSharedValueId).
fn is_ref_or_ref_like_mutable_type(ty: &Type) -> bool {
    match ty {
        Type::Object { shape_id: Some(id) } => {
            id == "BuiltInUseRefId" || id == "ReanimatedSharedValueId"
        }
        _ => false,
    }
}

fn is_ref_like_name(name: &str) -> bool {
    name == "ref" || name.ends_with("Ref")
}

fn is_ref_or_ref_like_mutable_place(place: &Place, enable_ref_like_names: bool) -> bool {
    if is_ref_or_ref_like_mutable_type(&place.identifier.type_) {
        return true;
    }
    if !enable_ref_like_names {
        return false;
    }
    if matches!(place.identifier.type_, Type::Poly | Type::TypeVar { .. })
        && let Some(name) = &place.identifier.name
    {
        return is_ref_like_name(name.value());
    }
    false
}

fn update_decl_global_status(
    decl_status: &mut HashMap<DeclarationId, (bool, bool)>,
    decl_id: DeclarationId,
    source_is_global: bool,
) -> bool {
    let entry = decl_status.entry(decl_id).or_insert((false, true));
    let prev = *entry;
    entry.0 = true;
    entry.1 &= source_is_global;
    prev != *entry
}

fn infer_definitely_global_alias_decls(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut global_values: HashSet<IdentifierId> = HashSet::new();
    let mut decl_status: HashMap<DeclarationId, (bool, bool)> = HashMap::new();

    loop {
        let mut changed = false;

        for (_bid, block) in &func.body.blocks {
            for phi in &block.phis {
                let mut all_global = true;
                let mut saw_operand = false;
                for operand in phi.operands.values() {
                    saw_operand = true;
                    if !global_values.contains(&operand.identifier.id) {
                        all_global = false;
                        break;
                    }
                }
                if saw_operand && all_global && global_values.insert(phi.place.identifier.id) {
                    changed = true;
                }
            }

            for instr in &block.instructions {
                let lvalue_id = instr.lvalue.identifier.id;

                let lvalue_is_global = match &instr.value {
                    InstructionValue::LoadGlobal { .. } => true,
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        global_values.contains(&place.identifier.id)
                    }
                    InstructionValue::TypeCastExpression { value, .. } => {
                        global_values.contains(&value.identifier.id)
                    }
                    InstructionValue::Ternary {
                        consequent,
                        alternate,
                        ..
                    } => {
                        global_values.contains(&consequent.identifier.id)
                            && global_values.contains(&alternate.identifier.id)
                    }
                    InstructionValue::LogicalExpression { left, right, .. } => {
                        global_values.contains(&left.identifier.id)
                            && global_values.contains(&right.identifier.id)
                    }
                    _ => false,
                };

                if lvalue_is_global && global_values.insert(lvalue_id) {
                    changed = true;
                }

                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        let source_is_global = global_values.contains(&value.identifier.id);
                        if update_decl_global_status(
                            &mut decl_status,
                            lvalue.place.identifier.declaration_id,
                            source_is_global,
                        ) {
                            changed = true;
                        }
                        if source_is_global && global_values.insert(lvalue.place.identifier.id) {
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
        }

        if !changed {
            break;
        }
    }

    decl_status
        .into_iter()
        .filter_map(|(decl_id, (seen_assignment, all_global))| {
            if seen_assignment && all_global {
                Some(decl_id)
            } else {
                None
            }
        })
        .collect()
}

/// Check if an operand with Freeze effect has a tracked mutation and emit a diagnostic if so.
fn check_operand(
    operand: &Place,
    context_mutation_effects: &HashMap<IdentifierId, TrackedMutation>,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    if operand.effect == Effect::Freeze
        && let Some(effect) = context_mutation_effects.get(&operand.identifier.id)
    {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[DEBUG_BAILOUT_REASON][no_freezing_known_mutable] freeze_operand id={} decl={} name={} type={:?} tracked_mutation id={} decl={} name={} type={:?}",
                operand.identifier.id.0,
                operand.identifier.declaration_id.0,
                operand
                    .identifier
                    .name
                    .as_ref()
                    .map_or_else(|| "_".to_string(), |name| name.value().to_string()),
                operand.identifier.type_,
                effect.value.identifier.id.0,
                effect.value.identifier.declaration_id.0,
                effect
                    .value
                    .identifier
                    .name
                    .as_ref()
                    .map_or_else(|| "_".to_string(), |name| name.value().to_string()),
                effect.value.identifier.type_,
            );
        }
        push_mutable_function_error(effect, &operand.loc, diagnostics);
    }
}

/// Validates that functions with known mutations cannot be passed where a frozen value is expected.
///
/// This validation runs unconditionally (not gated by any config flag) in the upstream pipeline.
pub fn validate_no_freezing_known_mutable_functions(
    func: &HIRFunction,
) -> Result<(), CompilerError> {
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();
    let mut context_mutation_effects: HashMap<IdentifierId, TrackedMutation> = HashMap::new();
    let definitely_global_alias_decls = infer_definitely_global_alias_decls(func);
    let enable_ref_like_names = func.env.config().enable_treat_ref_like_identifiers_as_refs;

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier.id;
            match &instr.value {
                InstructionValue::LoadLocal { place, .. } => {
                    // Propagate tracked mutations through loads
                    if let Some(effect) = context_mutation_effects.get(&place.identifier.id) {
                        let effect = effect.clone();
                        context_mutation_effects.insert(lvalue_id, effect);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    // Propagate tracked mutations through stores
                    if let Some(effect) = context_mutation_effects.get(&value.identifier.id) {
                        let effect = effect.clone();
                        context_mutation_effects.insert(lvalue_id, effect.clone());
                        context_mutation_effects.insert(lvalue.place.identifier.id, effect);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    if let Some(ref aliasing_effects) = lowered_func.func.aliasing_effects {
                        let context: HashSet<IdentifierId> = lowered_func
                            .func
                            .context
                            .iter()
                            .map(|p| p.identifier.id)
                            .collect();

                        'effects: for effect in aliasing_effects {
                            match effect {
                                AliasingEffect::Mutate { value, .. }
                                | AliasingEffect::MutateTransitive { value, .. } => {
                                    let is_ref_like = is_ref_or_ref_like_mutable_place(
                                        value,
                                        enable_ref_like_names,
                                    );
                                    if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                                        eprintln!(
                                            "[DEBUG_BAILOUT_REASON][no_freezing_known_mutable] effect={:?} lvalue_id={} value_id={} value_decl={} value_name={} value_type={:?} context_contains={} ref_like={}",
                                            effect,
                                            lvalue_id.0,
                                            value.identifier.id.0,
                                            value.identifier.declaration_id.0,
                                            value.identifier.name.as_ref().map_or_else(
                                                || "_".to_string(),
                                                |name| name.value().to_string()
                                            ),
                                            value.identifier.type_,
                                            context.contains(&value.identifier.id),
                                            is_ref_like
                                        );
                                    }
                                    if let Some(known_mutation) =
                                        context_mutation_effects.get(&value.identifier.id)
                                    {
                                        let known = known_mutation.clone();
                                        context_mutation_effects.insert(lvalue_id, known);
                                    } else if context.contains(&value.identifier.id) && !is_ref_like
                                    {
                                        if definitely_global_alias_decls
                                            .contains(&value.identifier.declaration_id)
                                        {
                                            if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                                                eprintln!(
                                                    "[DEBUG_BAILOUT_REASON][no_freezing_known_mutable] skip global-alias-capture id={} decl={} name={}",
                                                    value.identifier.id.0,
                                                    value.identifier.declaration_id.0,
                                                    value.identifier.name.as_ref().map_or_else(
                                                        || "_".to_string(),
                                                        |name| name.value().to_string()
                                                    ),
                                                );
                                            }
                                            continue;
                                        }
                                        context_mutation_effects.insert(
                                            lvalue_id,
                                            TrackedMutation {
                                                value: value.clone(),
                                            },
                                        );
                                        break 'effects;
                                    }
                                }
                                AliasingEffect::MutateConditionally { value, .. }
                                | AliasingEffect::MutateTransitiveConditionally { value, .. } => {
                                    if let Some(known_mutation) =
                                        context_mutation_effects.get(&value.identifier.id)
                                    {
                                        let known = known_mutation.clone();
                                        context_mutation_effects.insert(lvalue_id, known);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {
                    // For all other instruction kinds, check operands for freeze effects
                    for_each_instruction_operand(instr, |operand| {
                        check_operand(operand, &context_mutation_effects, &mut diagnostics);
                    });
                }
            }
        }

        // Also check terminal operands
        for_each_terminal_operand(&block.terminal, |operand| {
            check_operand(operand, &context_mutation_effects, &mut diagnostics);
        });
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Cannot pass mutable functions where frozen values are expected".to_string(),
            diagnostics,
        }))
    }
}
