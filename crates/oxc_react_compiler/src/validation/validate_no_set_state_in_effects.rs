//! Validates against calling setState in the body of an effect (useEffect and friends),
//! while allowing calling setState in callbacks scheduled by the effect.
//!
//! Port of `ValidateNoSetStateInEffects.ts` from upstream React Compiler.
//!
//! Calling setState during execution of a useEffect triggers a re-render, which is
//! often bad for performance and frequently has more efficient and straightforward
//! alternatives. See https://react.dev/learn/you-might-not-need-an-effect for examples.
//!
//! This pass runs on the **HIR** (CFG form).
//! It is gated behind `env.config.validateNoSetStateInEffects`.
//! Upstream uses `env.logErrors()` — errors are logged but do NOT bail compilation.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::*;
use crate::hir::visitors::for_each_instruction_operand;

/// Checks if an identifier's type indicates it is a useEffect / useLayoutEffect /
/// useInsertionEffect hook function.
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

/// Checks if an identifier's type indicates it is a useRef return value.
fn is_use_ref_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInUseRefId")
}

/// Checks if an identifier's type indicates it is a ref `.current` value.
fn is_ref_value_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInRefValue")
}

/// Validates that setState is not called synchronously within an effect body.
///
/// Returns `Err` with diagnostics if violations are found. The caller (pipeline)
/// should use `let _ = ...` to swallow the error, matching upstream `env.logErrors()`.
pub fn validate_no_set_state_in_effects(
    func: &HIRFunction,
    enable_allow_set_state_from_refs_in_effects: bool,
) -> Result<(), CompilerError> {
    // Map from identifier id -> source Place for setState functions
    let mut set_state_functions: HashMap<IdentifierId, Place> = HashMap::new();
    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            let lvalue_id = instr.lvalue.identifier.id;
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if is_set_state_type(&place.identifier)
                        || set_state_functions.contains_key(&place.identifier.id)
                    {
                        set_state_functions.insert(lvalue_id, place.clone());
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if is_set_state_type(&value.identifier)
                        || set_state_functions.contains_key(&value.identifier.id)
                    {
                        set_state_functions.insert(lvalue.place.identifier.id, value.clone());
                        set_state_functions.insert(lvalue_id, value.clone());
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    // Quick check: does this function expression reference a setState?
                    let has_set_state_operand = {
                        let mut found = false;
                        for_each_instruction_operand(instr, |operand| {
                            if is_set_state_type(&operand.identifier)
                                || set_state_functions.contains_key(&operand.identifier.id)
                            {
                                found = true;
                            }
                        });
                        found
                    };

                    if has_set_state_operand
                        && let Some(callee) = get_set_state_call(
                            &lowered_func.func,
                            &set_state_functions,
                            enable_allow_set_state_from_refs_in_effects,
                        )
                    {
                        set_state_functions.insert(lvalue_id, callee);
                    }
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    if is_use_effect_hook_type(&callee.identifier) {
                        check_effect_call(args, &set_state_functions, &mut diagnostics);
                    }
                }
                InstructionValue::MethodCall { receiver, args, .. } => {
                    // Upstream uses `receiver` for MethodCall callee check
                    if is_use_effect_hook_type(&receiver.identifier) {
                        check_effect_call(args, &set_state_functions, &mut diagnostics);
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
            reason: "setState called synchronously in an effect".to_string(),
            diagnostics,
        }))
    }
}

/// Check if the first argument to useEffect/useLayoutEffect/useInsertionEffect
/// is a function that calls setState.
fn check_effect_call(
    args: &[Argument],
    set_state_functions: &HashMap<IdentifierId, Place>,
    diagnostics: &mut Vec<CompilerDiagnostic>,
) {
    if let Some(Argument::Place(arg)) = args.first()
        && let Some(set_state_place) = set_state_functions.get(&arg.identifier.id)
    {
        diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: format!(
                    "Calling setState synchronously within an effect can trigger cascading renders. \
                     Effects are intended to synchronize state between React and external systems such as \
                     manually updating the DOM, state management libraries, or other platform APIs. \
                     In general, the body of an effect should do one or both of the following:\n\
                     * Update external systems with the latest state from React.\n\
                     * Subscribe for updates from some external system, calling setState in a callback \
                     function when external state changes.\n\n\
                     Calling setState synchronously within an effect body causes cascading renders that \
                     can hurt performance, and is not recommended. \
                     (https://react.dev/learn/you-might-not-need-an-effect) \
                     [{:?}]",
                    set_state_place.loc
                ),
                category: Some(ErrorCategory::EffectSetState),
            });
    }
}

/// Walks the inner function body looking for a direct `CallExpression` where the callee
/// is a setState function. Returns the callee Place if found, or None.
///
/// If `enable_allow_set_state_from_refs_in_effects` is true, setState calls where the
/// argument is derived from a ref are exempted.
fn get_set_state_call(
    func: &HIRFunction,
    set_state_functions: &HashMap<IdentifierId, Place>,
    enable_allow_set_state_from_refs_in_effects: bool,
) -> Option<Place> {
    let mut ref_derived_values: HashSet<IdentifierId> = HashSet::new();
    // Also track setState through loads within the inner function
    let mut inner_set_state: HashMap<IdentifierId, Place> = HashMap::new();

    // Seed inner_set_state from context captures
    for operand in &func.context {
        if is_set_state_type(&operand.identifier)
            || set_state_functions.contains_key(&operand.identifier.id)
        {
            let place = Place {
                identifier: operand.identifier.clone(),
                effect: Effect::Unknown,
                reactive: false,
                loc: operand.loc.clone(),
            };
            inner_set_state.insert(operand.identifier.id, place);
        }
    }

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if enable_allow_set_state_from_refs_in_effects {
                let is_derived_from_ref = |place: &Place| -> bool {
                    ref_derived_values.contains(&place.identifier.id)
                        || is_use_ref_type(&place.identifier)
                        || is_ref_value_type(&place.identifier)
                };

                let mut has_ref_operand = false;
                for_each_instruction_operand(instr, |operand| {
                    if is_derived_from_ref(operand) {
                        has_ref_operand = true;
                    }
                });

                if has_ref_operand {
                    // Mark all lvalues as ref-derived
                    ref_derived_values.insert(instr.lvalue.identifier.id);
                    // Also handle StoreLocal/StoreContext lvalues
                    match &instr.value {
                        InstructionValue::StoreLocal { lvalue, .. }
                        | InstructionValue::StoreContext { lvalue, .. } => {
                            ref_derived_values.insert(lvalue.place.identifier.id);
                        }
                        _ => {}
                    }
                }

                if let InstructionValue::PropertyLoad {
                    object, property, ..
                } = &instr.value
                    && matches!(property, PropertyLiteral::String(s) if s == "current")
                    && (is_use_ref_type(&object.identifier)
                        || is_ref_value_type(&object.identifier))
                {
                    ref_derived_values.insert(instr.lvalue.identifier.id);
                }
            }

            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if is_set_state_type(&place.identifier)
                        || set_state_functions.contains_key(&place.identifier.id)
                        || inner_set_state.contains_key(&place.identifier.id)
                    {
                        let source = set_state_functions
                            .get(&place.identifier.id)
                            .or_else(|| inner_set_state.get(&place.identifier.id))
                            .cloned()
                            .unwrap_or_else(|| place.clone());
                        inner_set_state.insert(instr.lvalue.identifier.id, source);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if is_set_state_type(&value.identifier)
                        || set_state_functions.contains_key(&value.identifier.id)
                        || inner_set_state.contains_key(&value.identifier.id)
                    {
                        let source = set_state_functions
                            .get(&value.identifier.id)
                            .or_else(|| inner_set_state.get(&value.identifier.id))
                            .cloned()
                            .unwrap_or_else(|| value.clone());
                        inner_set_state.insert(lvalue.place.identifier.id, source.clone());
                        inner_set_state.insert(instr.lvalue.identifier.id, source);
                    }
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    if is_set_state_type(&callee.identifier)
                        || set_state_functions.contains_key(&callee.identifier.id)
                        || inner_set_state.contains_key(&callee.identifier.id)
                    {
                        if enable_allow_set_state_from_refs_in_effects
                            && let Some(Argument::Place(arg)) = args.first()
                            && ref_derived_values.contains(&arg.identifier.id)
                        {
                            // Special case: setState of ref-derived value is allowed
                            return None;
                        }
                        // Found a setState call — return the callee place
                        return Some(callee.clone());
                    }
                }
                _ => {}
            }
        }
    }
    None
}
