//! Validates that useEffect is not used for derived computations which could/should
//! be performed in render.
//!
//! Port of `ValidateNoDerivedComputationsInEffects.ts` from upstream React Compiler.
//!
//! See https://react.dev/learn/you-might-not-need-an-effect#updating-state-based-on-props-or-state
//!
//! This pass runs on the **HIR** (CFG form).
//! It is gated behind `env.config.validateNoDerivedComputationsInEffects`.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;
use crate::hir::visitors::{for_each_instruction_operand, for_each_terminal_operand};

/// Checks if an identifier's type indicates it is a useEffect hook function.
fn is_use_effect_hook_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Function { shape_id: Some(shape_id), .. } if shape_id == "BuiltInUseEffectHookId"
    )
}

/// Checks if an identifier's type indicates it is a setState function.
fn is_set_state_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Function { shape_id: Some(shape_id), .. } if shape_id == "BuiltInSetState"
    )
}

/// Validates that useEffect is not used for derived computations.
pub fn validate_no_derived_computations_in_effects(
    func: &HIRFunction,
) -> Result<(), CompilerError> {
    let mut candidate_dependencies: HashMap<IdentifierId, Vec<ArrayElement>> = HashMap::new();
    let mut functions: HashMap<IdentifierId, &LoweredFunction> = HashMap::new();
    let mut locals: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier.id;
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    locals.insert(lvalue_id, place.identifier.id);
                }
                InstructionValue::ArrayExpression { elements, .. } => {
                    candidate_dependencies.insert(lvalue_id, elements.clone());
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    functions.insert(lvalue_id, lowered_func);
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    if is_use_effect_hook_type(&callee.identifier)
                        && args.len() == 2
                        && let (Argument::Place(arg0), Argument::Place(arg1)) = (&args[0], &args[1])
                    {
                        try_validate_effect_call(
                            arg0.identifier.id,
                            arg1.identifier.id,
                            &functions,
                            &candidate_dependencies,
                            &locals,
                            &mut diagnostics,
                        );
                    }
                }
                InstructionValue::MethodCall { property, args, .. } => {
                    if is_use_effect_hook_type(&property.identifier)
                        && args.len() == 2
                        && let (Argument::Place(arg0), Argument::Place(arg1)) = (&args[0], &args[1])
                    {
                        try_validate_effect_call(
                            arg0.identifier.id,
                            arg1.identifier.id,
                            &functions,
                            &candidate_dependencies,
                            &locals,
                            &mut diagnostics,
                        );
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
            reason: "Effect used for derived computation".to_string(),
            diagnostics,
        }))
    }
}

fn try_validate_effect_call(
    fn_arg_id: IdentifierId,
    deps_arg_id: IdentifierId,
    functions: &HashMap<IdentifierId, &LoweredFunction>,
    candidate_dependencies: &HashMap<IdentifierId, Vec<ArrayElement>>,
    locals: &HashMap<IdentifierId, IdentifierId>,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    // Resolve through locals (upstream resolves fn/deps args through LoadLocal)
    let resolved_fn_id = locals.get(&fn_arg_id).copied().unwrap_or(fn_arg_id);
    let resolved_deps_id = locals.get(&deps_arg_id).copied().unwrap_or(deps_arg_id);

    let effect_function = functions
        .get(&resolved_fn_id)
        .or_else(|| functions.get(&fn_arg_id));
    let deps = candidate_dependencies
        .get(&resolved_deps_id)
        .or_else(|| candidate_dependencies.get(&deps_arg_id));

    if let (Some(effect_function), Some(deps)) = (effect_function, deps)
        && !deps.is_empty()
        && deps.iter().all(|e| matches!(e, ArrayElement::Place(_)))
    {
        let dependencies: Vec<IdentifierId> = deps
            .iter()
            .filter_map(|e| {
                if let ArrayElement::Place(p) = e {
                    let id = p.identifier.id;
                    Some(*locals.get(&id).unwrap_or(&id))
                } else {
                    None
                }
            })
            .collect();

        validate_effect(&effect_function.func, &dependencies, diagnostics);
    }
}

fn validate_effect(
    effect_function: &HIRFunction,
    effect_deps: &[IdentifierId],
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    // Check context: every captured value must be either a setState or an effect dep
    for operand in &effect_function.context {
        if is_set_state_type(&operand.identifier) || effect_deps.contains(&operand.identifier.id) {
            continue;
        } else {
            // Captured something other than the effect dep or setState
            return;
        }
    }

    // Check that every effect dep is actually used in the function context
    for dep in effect_deps {
        if effect_function
            .context
            .iter()
            .all(|operand| operand.identifier.id != *dep)
        {
            // effect dep wasn't actually used in the function
            return;
        }
    }

    // Build a set of setState identifiers from context captures.
    // In our HIR, context variables loaded inside the function may have TypeVar types
    // instead of the original BuiltInSetState type, so we track setState IDs explicitly.
    let mut set_state_ids: HashSet<IdentifierId> = HashSet::new();
    for operand in &effect_function.context {
        if is_set_state_type(&operand.identifier) {
            set_state_ids.insert(operand.identifier.id);
        }
    }

    let mut seen_blocks: HashSet<BlockId> = HashSet::new();
    let mut values: HashMap<IdentifierId, Vec<IdentifierId>> = HashMap::new();
    for dep in effect_deps {
        values.insert(*dep, vec![*dep]);
    }

    let mut set_state_locations: Vec<SourceLocation> = Vec::new();

    for (_bid, block) in &effect_function.body.blocks {
        for pred in &block.preds {
            if !seen_blocks.contains(pred) {
                // skip if block has a back edge
                return;
            }
        }

        for phi in &block.phis {
            let mut aggregate_deps: HashSet<IdentifierId> = HashSet::new();
            for operand in phi.operands.values() {
                if let Some(deps) = values.get(&operand.identifier.id) {
                    for dep in deps {
                        aggregate_deps.insert(*dep);
                    }
                }
            }
            if !aggregate_deps.is_empty() {
                values.insert(
                    phi.place.identifier.id,
                    aggregate_deps.into_iter().collect(),
                );
            }
        }

        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::Primitive { .. }
                | InstructionValue::JSXText { .. }
                | InstructionValue::LoadGlobal { .. } => {}
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(deps) = values.get(&place.identifier.id) {
                        let deps = deps.clone();
                        values.insert(instr.lvalue.identifier.id, deps);
                    }
                    // Propagate setState tracking through loads
                    if set_state_ids.contains(&place.identifier.id) {
                        set_state_ids.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::ComputedLoad { .. }
                | InstructionValue::PropertyLoad { .. }
                | InstructionValue::BinaryExpression { .. }
                | InstructionValue::TemplateLiteral { .. }
                | InstructionValue::CallExpression { .. }
                | InstructionValue::MethodCall { .. } => {
                    let mut aggregate_deps: HashSet<IdentifierId> = HashSet::new();
                    for_each_instruction_operand(instr, |operand| {
                        if let Some(deps) = values.get(&operand.identifier.id) {
                            for dep in deps {
                                aggregate_deps.insert(*dep);
                            }
                        }
                    });
                    if !aggregate_deps.is_empty() {
                        values.insert(
                            instr.lvalue.identifier.id,
                            aggregate_deps.into_iter().collect(),
                        );
                    }

                    if let InstructionValue::CallExpression { callee, args, .. } = &instr.value
                        && (is_set_state_type(&callee.identifier)
                            || set_state_ids.contains(&callee.identifier.id))
                        && args.len() == 1
                        && matches!(&args[0], Argument::Place(_))
                        && let Argument::Place(arg) = &args[0]
                    {
                        if let Some(deps) = values.get(&arg.identifier.id) {
                            let unique_deps: HashSet<&IdentifierId> = deps.iter().collect();
                            if unique_deps.len() == effect_deps.len() {
                                set_state_locations.push(callee.loc.clone());
                            } else {
                                return;
                            }
                        } else {
                            return;
                        }
                    }
                }
                _ => {
                    return;
                }
            }
        }

        let mut has_dep_terminal_operand = false;
        for_each_terminal_operand(&block.terminal, |operand| {
            if values.contains_key(&operand.identifier.id) {
                has_dep_terminal_operand = true;
            }
        });
        if has_dep_terminal_operand {
            return;
        }

        seen_blocks.insert(block.id);
    }

    for _loc in &set_state_locations {
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: "Values derived from props and state should be calculated during render, not in an effect. (https://react.dev/learn/you-might-not-need-an-effect#updating-state-based-on-props-or-state)".to_string(),
            category: None,
        });
    }
}
