//! Validates that local variables aren't reassigned after they escape render scope.
//!
//! Port of `ValidateLocalsNotReassignedAfterRender.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Note: The upstream relies on `DeclareContext`/`StoreContext`/`LoadContext` instructions
//! which are created by `FindContextIdentifiers` + `BuildHIR` before any pipeline pass runs.
//! Our port doesn't have this pre-pass, so we detect context variables through:
//! - `DeclareContext`/`StoreContext` (if they exist from analyse_functions)
//! - `StoreGlobal` in inner functions targeting outer variable names
//! - Inner function `context` captures

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::*;
use crate::hir::visitors;
use crate::inference::infer_mutation_aliasing_effects::get_function_call_signature;

/// Validates that local variables aren't reassigned after render scope.
pub fn validate_locals_not_reassigned_after_render(
    func: &HIRFunction,
) -> Result<(), CompilerError> {
    // Collect all named variable declarations at the outer level.
    // These are potential "context variables" if they get captured by inner functions.
    let mut outer_vars: HashMap<String, IdentifierId> = HashMap::new();
    collect_outer_variable_names(func, &mut outer_vars);

    let mut context_variables: HashSet<IdentifierId> = HashSet::new();

    let reassignment =
        get_context_reassignment(func, &mut context_variables, &outer_vars, false, false)?;

    if let Some(place) = reassignment {
        let var_name = place_name(&place);
        return Err(CompilerError::Bail(BailOut {
            reason: format!("Cannot reassign {var_name} after render completes"),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: format!(
                    "Reassigning {var_name} after render has completed can cause \
                     inconsistent behavior on subsequent renders. Consider using state instead"
                ),
                category: Some(ErrorCategory::Immutability),
            }],
        }));
    }
    Ok(())
}

/// Collect all named variable declarations from a function.
/// Maps variable name -> IdentifierId for context variable detection.
fn collect_outer_variable_names(
    func: &HIRFunction,
    outer_vars: &mut HashMap<String, IdentifierId>,
) {
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if let Some(name) = &lvalue.place.identifier.name {
                        outer_vars.insert(name.value().to_string(), lvalue.place.identifier.id);
                    }
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if let Some(name) = &lvalue.place.identifier.name {
                        outer_vars
                            .entry(name.value().to_string())
                            .or_insert(lvalue.place.identifier.id);
                    }
                }
                _ => {}
            }
        }
    }
}

/// Recursively check a function for context variable reassignments.
///
/// Returns `Some(place)` if a reassignment site was found that should be an error.
fn get_context_reassignment(
    func: &HIRFunction,
    context_variables: &mut HashSet<IdentifierId>,
    outer_vars: &HashMap<String, IdentifierId>,
    is_function_expression: bool,
    is_async: bool,
) -> Result<Option<Place>, CompilerError> {
    let mut reassigning_functions: HashMap<IdentifierId, Place> = HashMap::new();
    let method_name_by_decl = collect_method_names(func);

    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                // DeclareContext: register context variables at top level
                InstructionValue::DeclareContext { lvalue, .. } => {
                    if !is_function_expression {
                        context_variables.insert(lvalue.place.identifier.id);
                    }
                }

                // FunctionExpression / ObjectMethod: recurse into nested functions
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    let inner_async = is_async || lowered_func.func.async_;

                    // Before recursing, register context variables from the inner function's
                    // context captures. Any captured outer variable that gets reassigned
                    // should be treated as a context variable.
                    for ctx_place in &lowered_func.func.context {
                        // Only treat captures as context variables if they resolve to a
                        // declaration from the current outer function. Inner-function locals
                        // can appear in `context` for hoisting/analysis, but they should not
                        // trigger outer render reassignment diagnostics.
                        let mut matched_outer = false;
                        if let Some(name) = &ctx_place.identifier.name
                            && let Some(&outer_id) = outer_vars.get(name.value())
                        {
                            context_variables.insert(outer_id);
                            matched_outer = true;
                        } else if outer_vars.values().any(|outer_id| {
                            ctx_place.identifier.declaration_id == DeclarationId(outer_id.0)
                        }) {
                            matched_outer = true;
                        }
                        if matched_outer {
                            context_variables.insert(ctx_place.identifier.id);
                        }
                    }

                    // Recursively check the inner function
                    let inner_result = get_context_reassignment(
                        &lowered_func.func,
                        context_variables,
                        outer_vars,
                        true,
                        inner_async,
                    )?;

                    let mut is_reassigning = inner_result.is_some();
                    let mut reassign_place = inner_result;

                    // Check if any operand is already a reassigning function
                    if !is_reassigning {
                        visitors::for_each_instruction_operand(instr, |operand| {
                            if let Some(place) = reassigning_functions.get(&operand.identifier.id) {
                                is_reassigning = true;
                                if reassign_place.is_none() {
                                    reassign_place = Some(place.clone());
                                }
                            }
                        });
                    }

                    if is_reassigning && let Some(ref place) = reassign_place {
                        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                            eprintln!(
                                "[REASSIGN_VALIDATE] mark-fn id={} reassignment={}",
                                instr.lvalue.identifier.id.0,
                                place_name(place)
                            );
                        }
                        // Async functions that reassign always error immediately
                        if inner_async {
                            let var_name = place_name(place);
                            return Err(CompilerError::Bail(BailOut {
                                reason: format!("Cannot reassign {var_name} in async function"),
                                diagnostics: vec![CompilerDiagnostic {
                                    severity: DiagnosticSeverity::InvalidReact,
                                    message: "Reassigning a variable in an async function can \
                                             cause inconsistent behavior on subsequent renders. \
                                             Consider using state instead"
                                        .to_string(),
                                    category: Some(ErrorCategory::Immutability),
                                }],
                            }));
                        }
                        reassigning_functions.insert(instr.lvalue.identifier.id, place.clone());
                    }
                }

                // StoreLocal: propagate reassignment status + check context variable reassignment
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    // Check if this is a reassignment of a context variable in an inner function.
                    // This handles cases where StoreLocal is used instead of StoreContext
                    // (since our port doesn't always promote to StoreContext).
                    if is_function_expression
                        && context_variables.contains(&lvalue.place.identifier.id)
                    {
                        return Ok(Some(lvalue.place.clone()));
                    }

                    if let Some(place) = reassigning_functions.get(&value.identifier.id).cloned() {
                        reassigning_functions.insert(lvalue.place.identifier.id, place.clone());
                        reassigning_functions.insert(instr.lvalue.identifier.id, place);
                    }
                }

                // StoreGlobal in inner function: check if it targets a context variable
                InstructionValue::StoreGlobal { name, .. } => {
                    if is_function_expression
                        && let Some(&outer_id) = outer_vars.get(name.as_str())
                        && context_variables.contains(&outer_id)
                    {
                        // This is a reassignment of an outer context variable.
                        // Create a synthetic place for the error message.
                        let place = Place {
                            identifier: Identifier {
                                id: outer_id,
                                declaration_id: DeclarationId(outer_id.0),
                                name: Some(IdentifierName::Named(name.clone())),
                                mutable_range: MutableRange::default(),
                                scope: None,
                                type_: Type::Poly,
                                loc: instr.loc.clone(),
                            },
                            effect: Effect::Unknown,
                            reactive: false,
                            loc: instr.loc.clone(),
                        };
                        return Ok(Some(place));
                    }
                }

                // LoadLocal/LoadContext: propagate reassignment status
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(reassign_place) =
                        reassigning_functions.get(&place.identifier.id).cloned()
                    {
                        reassigning_functions.insert(instr.lvalue.identifier.id, reassign_place);
                    }
                }

                // StoreContext: check for direct reassignment or propagation
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    // Check if this is a direct reassignment of a context variable
                    if is_function_expression
                        && context_variables.contains(&lvalue.place.identifier.id)
                    {
                        return Ok(Some(lvalue.place.clone()));
                    }

                    // At top level, register as context variable
                    if !is_function_expression {
                        context_variables.insert(lvalue.place.identifier.id);
                    }

                    // Propagate reassignment status from value
                    if let Some(place) = reassigning_functions.get(&value.identifier.id).cloned() {
                        reassigning_functions.insert(lvalue.place.identifier.id, place.clone());
                        reassigning_functions.insert(instr.lvalue.identifier.id, place);
                    }
                }

                // Default: check operands for reassigning functions with Freeze effect
                _ => {
                    let mut found_error: Option<Place> = None;
                    let mut should_propagate = false;
                    let mut propagate_place: Option<Place> = None;

                    let mut visit_operand = |operand: &Place| {
                        if let Some(place) = reassigning_functions.get(&operand.identifier.id) {
                            if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                                eprintln!(
                                    "[REASSIGN_VALIDATE] operand id={} effect={:?} reassignment={} instr={}",
                                    operand.identifier.id.0,
                                    operand.effect,
                                    place_name(place),
                                    instruction_kind_name(&instr.value)
                                );
                            }
                            if operand.effect == Effect::Freeze {
                                // Error: reassigning function is being frozen
                                if found_error.is_none() {
                                    found_error = Some(place.clone());
                                }
                            } else {
                                // Propagate to lvalues
                                should_propagate = true;
                                if propagate_place.is_none() {
                                    propagate_place = Some(place.clone());
                                }
                            }
                        }
                    };

                    match &instr.value {
                        InstructionValue::CallExpression { callee, .. } => {
                            if get_function_call_signature(&callee.identifier.type_)
                                .is_some_and(|signature| signature.no_alias)
                            {
                                visit_operand(callee);
                            } else {
                                visitors::for_each_instruction_operand(instr, &mut visit_operand);
                            }
                        }
                        InstructionValue::MethodCall {
                            receiver, property, ..
                        } => {
                            let has_no_alias_signature =
                                get_function_call_signature(&property.identifier.type_)
                                    .is_some_and(|signature| signature.no_alias)
                                    || method_call_is_no_alias(
                                        receiver,
                                        property,
                                        &method_name_by_decl,
                                    );
                            if has_no_alias_signature {
                                visit_operand(receiver);
                                visit_operand(property);
                            } else {
                                visitors::for_each_instruction_operand(instr, &mut visit_operand);
                            }
                        }
                        InstructionValue::TaggedTemplateExpression { tag, .. } => {
                            if get_function_call_signature(&tag.identifier.type_)
                                .is_some_and(|signature| signature.no_alias)
                            {
                                visit_operand(tag);
                            } else {
                                visitors::for_each_instruction_operand(instr, &mut visit_operand);
                            }
                        }
                        _ => {
                            visitors::for_each_instruction_operand(instr, &mut visit_operand);
                        }
                    }

                    if let Some(error_place) = found_error {
                        return Ok(Some(error_place));
                    }

                    if should_propagate && let Some(place) = propagate_place {
                        visitors::for_each_instruction_lvalue(instr, |lv| {
                            reassigning_functions.insert(lv.identifier.id, place.clone());
                        });
                    }
                }
            }
        }

        // Check terminal operands
        let mut terminal_error: Option<Place> = None;
        visitors::for_each_terminal_operand(&block.terminal, |operand| {
            if let Some(place) = reassigning_functions.get(&operand.identifier.id)
                && terminal_error.is_none()
            {
                terminal_error = Some(place.clone());
            }
        });
        if let Some(error_place) = terminal_error {
            return Ok(Some(error_place));
        }
    }

    Ok(None)
}

fn collect_method_names(func: &HIRFunction) -> HashMap<DeclarationId, String> {
    let mut method_name_by_decl = HashMap::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::PropertyLoad {
                property: PropertyLiteral::String(name),
                ..
            } = &instr.value
            {
                method_name_by_decl.insert(instr.lvalue.identifier.declaration_id, name.clone());
            }
        }
    }
    method_name_by_decl
}

fn method_call_is_no_alias(
    receiver: &Place,
    property: &Place,
    method_name_by_decl: &HashMap<DeclarationId, String>,
) -> bool {
    use crate::hir::globals::GlobalRegistry;
    use crate::hir::object_shape::PropertyType;

    let method_name = match property.identifier.name.as_ref() {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            Some(name.clone())
        }
        None => method_name_by_decl
            .get(&property.identifier.declaration_id)
            .cloned(),
    };

    let receiver_shape_id = match &receiver.identifier.type_ {
        Type::Object {
            shape_id: Some(shape_id),
        }
        | Type::Function {
            shape_id: Some(shape_id),
            ..
        } => Some(shape_id.as_str()),
        _ => None,
    };

    let (Some(shape_id), Some(method_name)) = (receiver_shape_id, method_name) else {
        return false;
    };

    let globals = GlobalRegistry::new();
    match globals.shapes.get_property(shape_id, &method_name) {
        Some(PropertyType::Function(signature)) => signature.no_alias,
        _ => false,
    }
}

/// Extract a display name for an identifier in a Place.
fn place_name(place: &Place) -> String {
    match &place.identifier.name {
        Some(IdentifierName::Named(name)) => format!("`{name}`"),
        Some(IdentifierName::Promoted(name)) => format!("`{name}`"),
        None => "variable".to_string(),
    }
}

fn instruction_kind_name(value: &InstructionValue) -> &'static str {
    match value {
        InstructionValue::Primitive { .. } => "Primitive",
        InstructionValue::BinaryExpression { .. } => "BinaryExpression",
        InstructionValue::LoadLocal { .. } => "LoadLocal",
        InstructionValue::StoreLocal { .. } => "StoreLocal",
        InstructionValue::LoadContext { .. } => "LoadContext",
        InstructionValue::StoreContext { .. } => "StoreContext",
        InstructionValue::LoadGlobal { .. } => "LoadGlobal",
        InstructionValue::StoreGlobal { .. } => "StoreGlobal",
        InstructionValue::PropertyLoad { .. } => "PropertyLoad",
        InstructionValue::PropertyStore { .. } => "PropertyStore",
        InstructionValue::ComputedLoad { .. } => "ComputedLoad",
        InstructionValue::ComputedStore { .. } => "ComputedStore",
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::FunctionExpression { .. } => "FunctionExpression",
        InstructionValue::ObjectExpression { .. } => "ObjectExpression",
        InstructionValue::ArrayExpression { .. } => "ArrayExpression",
        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
        InstructionValue::DeclareContext { .. } => "DeclareContext",
        InstructionValue::NewExpression { .. } => "NewExpression",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_place(id: u32) -> Place {
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

    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_test_place(0),
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
    fn test_empty_function_passes() {
        let func = make_hir_function(vec![]);
        assert!(validate_locals_not_reassigned_after_render(&func).is_ok());
    }
}
