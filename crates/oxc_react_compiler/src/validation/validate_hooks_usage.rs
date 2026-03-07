//! Validates that hooks are used according to the Rules of Hooks.
//!
//! Port of `ValidateHooksUsage.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This validates:
//! - Known hooks may only be called unconditionally, and cannot be used as first-class values.
//! - Potential hooks may be referenced as first-class values, with the exception that they
//!   may not appear as the callee of a conditional call.
//! - Hooks must not be called within nested function expressions.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;
use crate::hir::visitors::{
    for_each_instruction_lvalue, for_each_instruction_operand, for_each_terminal_operand,
};

/// Represents the possible kinds of value which may be stored at a given Place
/// during abstract interpretation. The kinds form a lattice, with earlier items
/// taking precedence over later items (see `join_kinds()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A potential/known hook which was already used in an invalid way.
    Error,
    /// A known hook (e.g., from LoadGlobal whose type was inferred as a hook).
    KnownHook,
    /// A potential hook (e.g., lvalues where the name is hook-like).
    PotentialHook,
    /// A global value that is not a hook.
    Global,
    /// All other values (local variables).
    Local,
}

fn join_kinds(a: Kind, b: Kind) -> Kind {
    if a == Kind::Error || b == Kind::Error {
        Kind::Error
    } else if a == Kind::KnownHook || b == Kind::KnownHook {
        Kind::KnownHook
    } else if a == Kind::PotentialHook || b == Kind::PotentialHook {
        Kind::PotentialHook
    } else if a == Kind::Global || b == Kind::Global {
        Kind::Global
    } else {
        Kind::Local
    }
}

fn hook_name_for_shape_id(shape_id: &str) -> Option<&'static str> {
    match shape_id {
        "BuiltInUseStateHookId" => Some("useState"),
        "BuiltInUseReducerHookId" => Some("useReducer"),
        "BuiltInUseContextHookId" => Some("useContext"),
        "BuiltInUseRefHookId" => Some("useRef"),
        "BuiltInUseMemoHookId" => Some("useMemo"),
        "BuiltInUseCallbackHookId" => Some("useCallback"),
        "BuiltInUseEffectHookId" => Some("useEffect"),
        "BuiltInUseLayoutEffectHookId" => Some("useLayoutEffect"),
        "BuiltInUseInsertionEffectHookId" => Some("useInsertionEffect"),
        "BuiltInUseTransitionHookId" => Some("useTransition"),
        "BuiltInUseImperativeHandleHookId" => Some("useImperativeHandle"),
        "BuiltInUseActionStateHookId" => Some("useActionState"),
        _ => None,
    }
}

fn hook_desc_for_place(place: &Place) -> String {
    if let Type::Function {
        shape_id: Some(shape_id),
        ..
    } = &place.identifier.type_
        && let Some(name) = hook_name_for_shape_id(shape_id)
    {
        return name.to_string();
    }
    place
        .identifier
        .name
        .as_ref()
        .map_or("hook".to_string(), |n| {
            if Environment::is_hook_name(n.value()) {
                match n.value() {
                    "useState"
                    | "useReducer"
                    | "useContext"
                    | "useRef"
                    | "useMemo"
                    | "useCallback"
                    | "useEffect"
                    | "useLayoutEffect"
                    | "useInsertionEffect"
                    | "useTransition"
                    | "useImperativeHandle"
                    | "useActionState" => n.value().to_string(),
                    _ => "hook".to_string(),
                }
            } else {
                n.value().to_string()
            }
        })
}

fn is_use_operator_place(
    place: &Place,
    id_string_values: Option<&HashMap<IdentifierId, String>>,
) -> bool {
    if place
        .identifier
        .name
        .as_ref()
        .is_some_and(|n| n.value() == "use")
    {
        return true;
    }
    id_string_values
        .and_then(|map| map.get(&place.identifier.id))
        .is_some_and(|name| name == "use")
}

fn is_known_hook_load_global(binding: &NonLocalBinding, lvalue: &Place) -> bool {
    if let Type::Function {
        shape_id: Some(shape_id),
        ..
    } = &lvalue.identifier.type_
        && hook_name_for_shape_id(shape_id).is_some()
    {
        return true;
    }

    match binding {
        NonLocalBinding::ImportSpecifier { name, imported, .. } => {
            Environment::is_hook_name(imported) || Environment::is_hook_name(name)
        }
        NonLocalBinding::ImportDefault { name, .. }
        | NonLocalBinding::ImportNamespace { name, .. }
        | NonLocalBinding::ModuleLocal { name }
        | NonLocalBinding::Global { name } => Environment::is_hook_name(name),
    }
}

fn is_use_operator_binding(binding: &NonLocalBinding) -> bool {
    match binding {
        NonLocalBinding::ImportSpecifier { name, imported, .. } => {
            name == "use" || imported == "use"
        }
        NonLocalBinding::ImportDefault { name, .. }
        | NonLocalBinding::ImportNamespace { name, .. }
        | NonLocalBinding::ModuleLocal { name }
        | NonLocalBinding::Global { name } => name == "use",
    }
}

fn record_conditional_hook_error(
    value_kinds: &mut HashMap<IdentifierId, Kind>,
    diagnostics: &mut Vec<CompilerDiagnostic>,
    place: &Place,
) {
    value_kinds.insert(place.identifier.id, Kind::Error);
    diagnostics.push(CompilerDiagnostic {
        severity: DiagnosticSeverity::InvalidReact,
        message: "Hooks must always be called in a consistent order, and may not be called \
                  conditionally. See the Rules of Hooks \
                  (https://react.dev/warnings/invalid-hook-call-warning)"
            .to_string(),
    });
}

fn resolve_method_callee_kind(
    value_kinds: &HashMap<IdentifierId, Kind>,
    id_string_values: &HashMap<IdentifierId, String>,
    receiver: &Place,
    property: &Place,
) -> Kind {
    let get_kind_for_place = |place: &Place| -> Kind {
        let known_kind = value_kinds.get(&place.identifier.id).copied();
        if place
            .identifier
            .name
            .as_ref()
            .is_some_and(|n| Environment::is_hook_name(n.value()))
        {
            join_kinds(known_kind.unwrap_or(Kind::Local), Kind::PotentialHook)
        } else {
            known_kind.unwrap_or(Kind::Local)
        }
    };

    let mut callee_kind = get_kind_for_place(property);
    if callee_kind == Kind::Local
        && let Some(prop_str) = id_string_values.get(&property.identifier.id)
        && Environment::is_hook_name(prop_str)
    {
        let receiver_kind = get_kind_for_place(receiver);
        callee_kind = match receiver_kind {
            Kind::Global => Kind::KnownHook,
            Kind::KnownHook => Kind::KnownHook,
            Kind::PotentialHook => Kind::PotentialHook,
            Kind::Local => Kind::PotentialHook,
            Kind::Error => Kind::Error,
        };
    }
    callee_kind
}

/// Validates that the function honors the Rules of Hooks.
///
/// Returns `Ok(())` if valid, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_hooks_usage(func: &HIRFunction) -> Result<(), CompilerError> {
    let unconditional_blocks = compute_unconditional_blocks(func);
    let reachable_blocks = compute_reachable_blocks(func);

    let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();
    let mut value_kinds: HashMap<IdentifierId, Kind> = HashMap::new();
    // Track string literal values for temporaries created from Primitive::String.
    // Needed to resolve MethodCall property names (e.g., `local.useFoo()` lowers to
    // a MethodCall whose property is a temporary backed by Primitive::String("useFoo")).
    let mut id_string_values: HashMap<IdentifierId, String> = HashMap::new();
    let mut use_operator_values: HashSet<IdentifierId> = HashSet::new();
    // Helper closures replaced by inline functions using the map
    let get_kind_for_place = |value_kinds: &HashMap<IdentifierId, Kind>, place: &Place| -> Kind {
        let known_kind = value_kinds.get(&place.identifier.id).copied();
        if place
            .identifier
            .name
            .as_ref()
            .is_some_and(|n| Environment::is_hook_name(n.value()))
        {
            join_kinds(known_kind.unwrap_or(Kind::Local), Kind::PotentialHook)
        } else {
            known_kind.unwrap_or(Kind::Local)
        }
    };

    let visit_place = |value_kinds: &HashMap<IdentifierId, Kind>,
                       diagnostics: &mut Vec<CompilerDiagnostic>,
                       place: &Place| {
        let kind = value_kinds.get(&place.identifier.id).copied();
        if kind == Some(Kind::KnownHook) {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: "Hooks may not be referenced as normal values, they must be called. \
                          See https://react.dev/reference/rules/react-calls-components-and-hooks#never-pass-around-hooks-as-regular-values"
                    .to_string(),
            });
        }
    };

    // Initialize params
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        let kind = get_kind_for_place(&value_kinds, place);
        value_kinds.insert(place.identifier.id, kind);
    }

    // Process blocks
    for (_bid, block) in &func.body.blocks {
        if !reachable_blocks.contains(&block.id) {
            continue;
        }
        // Process phis
        for phi in &block.phis {
            let mut kind = if phi
                .place
                .identifier
                .name
                .as_ref()
                .is_some_and(|n| Environment::is_hook_name(n.value()))
            {
                Kind::PotentialHook
            } else {
                Kind::Local
            };
            for operand in phi.operands.values() {
                if let Some(&operand_kind) = value_kinds.get(&operand.identifier.id) {
                    kind = join_kinds(kind, operand_kind);
                }
            }
            value_kinds.insert(phi.place.identifier.id, kind);
        }

        // Process instructions
        for instr in &block.instructions {
            // Track string literal values for method property resolution
            if let InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } = &instr.value
            {
                id_string_values.insert(instr.lvalue.identifier.id, s.clone());
            }
            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    // Globals are the one source of known hooks
                    if is_known_hook_load_global(binding, &instr.lvalue) {
                        value_kinds.insert(instr.lvalue.identifier.id, Kind::KnownHook);
                    } else {
                        value_kinds.insert(instr.lvalue.identifier.id, Kind::Global);
                    }
                    if is_use_operator_binding(binding) {
                        use_operator_values.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::LoadContext { place, .. }
                | InstructionValue::LoadLocal { place, .. } => {
                    visit_place(&value_kinds, &mut diagnostics, place);
                    let kind = get_kind_for_place(&value_kinds, place);
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                    if use_operator_values.contains(&place.identifier.id) {
                        use_operator_values.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    visit_place(&value_kinds, &mut diagnostics, value);
                    let kind = join_kinds(
                        get_kind_for_place(&value_kinds, value),
                        get_kind_for_place(&value_kinds, &lvalue.place),
                    );
                    value_kinds.insert(lvalue.place.identifier.id, kind);
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                    if use_operator_values.contains(&value.identifier.id) {
                        use_operator_values.insert(lvalue.place.identifier.id);
                        use_operator_values.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::ComputedLoad { object, .. } => {
                    visit_place(&value_kinds, &mut diagnostics, object);
                    let kind = get_kind_for_place(&value_kinds, object);
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, join_kinds(lvalue_kind, kind));
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    let object_kind = get_kind_for_place(&value_kinds, object);
                    let is_hook_property = match property {
                        PropertyLiteral::String(s) => Environment::is_hook_name(s),
                        PropertyLiteral::Number(_) => false,
                    };

                    let kind = match object_kind {
                        Kind::Error => Kind::Error,
                        Kind::KnownHook => {
                            if is_hook_property {
                                Kind::KnownHook
                            } else {
                                Kind::Local
                            }
                        }
                        Kind::PotentialHook => Kind::PotentialHook,
                        Kind::Global => {
                            if is_hook_property {
                                Kind::KnownHook
                            } else {
                                Kind::Global
                            }
                        }
                        Kind::Local => {
                            if is_hook_property {
                                Kind::PotentialHook
                            } else {
                                Kind::Local
                            }
                        }
                    };
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                }
                InstructionValue::CallExpression {
                    callee, optional, ..
                } => {
                    let callee_kind = get_kind_for_place(&value_kinds, callee);
                    let is_hook_callee =
                        callee_kind == Kind::KnownHook || callee_kind == Kind::PotentialHook;
                    let is_use_operator = use_operator_values.contains(&callee.identifier.id)
                        || is_use_operator_place(callee, None);
                    let is_conditional_call =
                        *optional || !unconditional_blocks.contains(&block.id);
                    if is_hook_callee && is_conditional_call && !is_use_operator {
                        record_conditional_hook_error(&mut value_kinds, &mut diagnostics, callee);
                    } else if callee_kind == Kind::PotentialHook {
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: "Hooks must be the same function on every render, but this \
                                      value may change over time to a different function. See \
                                      https://react.dev/reference/rules/react-calls-components-and-hooks#dont-dynamically-use-hooks"
                                .to_string(),
                        });
                    }
                    // Visit operands except callee
                    for_each_instruction_operand(instr, |operand| {
                        if operand.identifier.id != callee.identifier.id {
                            visit_place(&value_kinds, &mut diagnostics, operand);
                        }
                    });
                    // Default: set lvalue kind from name
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    receiver_optional,
                    call_optional,
                    ..
                } => {
                    let callee_kind = resolve_method_callee_kind(
                        &value_kinds,
                        &id_string_values,
                        receiver,
                        property,
                    );
                    let is_hook_callee =
                        callee_kind == Kind::KnownHook || callee_kind == Kind::PotentialHook;
                    let is_use_operator = is_use_operator_place(property, Some(&id_string_values));
                    let is_conditional_call = *receiver_optional
                        || *call_optional
                        || !unconditional_blocks.contains(&block.id);
                    if is_hook_callee && is_conditional_call && !is_use_operator {
                        record_conditional_hook_error(&mut value_kinds, &mut diagnostics, property);
                    } else if callee_kind == Kind::PotentialHook {
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: "Hooks must be the same function on every render, but this \
                                      value may change over time to a different function. See \
                                      https://react.dev/reference/rules/react-calls-components-and-hooks#dont-dynamically-use-hooks"
                                .to_string(),
                        });
                    }
                    // Visit operands except property
                    for_each_instruction_operand(instr, |operand| {
                        if operand.identifier.id != property.identifier.id {
                            visit_place(&value_kinds, &mut diagnostics, operand);
                        }
                    });
                    // Default: set lvalue kind from name
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::LogicalExpression { .. } => {
                    for_each_instruction_operand(instr, |operand| {
                        visit_place(&value_kinds, &mut diagnostics, operand);
                    });
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::Ternary { .. } => {
                    for_each_instruction_operand(instr, |operand| {
                        visit_place(&value_kinds, &mut diagnostics, operand);
                    });
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::Destructure { value, .. } => {
                    visit_place(&value_kinds, &mut diagnostics, value);
                    let object_kind = get_kind_for_place(&value_kinds, value);

                    for_each_instruction_lvalue(instr, |lvalue| {
                        let is_hook_property = lvalue
                            .identifier
                            .name
                            .as_ref()
                            .is_some_and(|n| Environment::is_hook_name(n.value()));
                        let kind = match object_kind {
                            Kind::Error => Kind::Error,
                            Kind::KnownHook => Kind::KnownHook,
                            Kind::PotentialHook => Kind::PotentialHook,
                            Kind::Global => {
                                if is_hook_property {
                                    Kind::KnownHook
                                } else {
                                    Kind::Global
                                }
                            }
                            Kind::Local => {
                                if is_hook_property {
                                    Kind::PotentialHook
                                } else {
                                    Kind::Local
                                }
                            }
                        };
                        value_kinds.insert(lvalue.identifier.id, kind);
                    });
                }
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    visit_function_expression(&mut diagnostics, &lowered_func.func);
                }
                _ => {
                    // Check usages of operands but do NOT flow properties
                    // from operands into the lvalues.
                    for_each_instruction_operand(instr, |operand| {
                        visit_place(&value_kinds, &mut diagnostics, operand);
                    });
                    for_each_instruction_lvalue(instr, |lvalue| {
                        let kind = get_kind_for_place(&value_kinds, lvalue);
                        value_kinds.insert(lvalue.identifier.id, kind);
                    });
                }
            }
        }

        // Check terminal operands
        for_each_terminal_operand(&block.terminal, |operand| {
            visit_place(&value_kinds, &mut diagnostics, operand);
        });
    }

    if diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Invalid hooks usage".to_string(),
            diagnostics,
        }))
    }
}

/// Visit nested function expressions to check that hooks are not called inside them.
fn visit_function_expression(diagnostics: &mut Vec<CompilerDiagnostic>, func: &HIRFunction) {
    let reachable_blocks = compute_reachable_blocks(func);
    let mut value_kinds: HashMap<IdentifierId, Kind> = HashMap::new();
    let mut id_string_values: HashMap<IdentifierId, String> = HashMap::new();

    let get_kind_for_place = |value_kinds: &HashMap<IdentifierId, Kind>, place: &Place| -> Kind {
        let known_kind = value_kinds.get(&place.identifier.id).copied();
        if place
            .identifier
            .name
            .as_ref()
            .is_some_and(|n| Environment::is_hook_name(n.value()))
        {
            join_kinds(known_kind.unwrap_or(Kind::Local), Kind::PotentialHook)
        } else {
            known_kind.unwrap_or(Kind::Local)
        }
    };

    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        let kind = get_kind_for_place(&value_kinds, place);
        value_kinds.insert(place.identifier.id, kind);
    }

    for (_bid, block) in &func.body.blocks {
        if !reachable_blocks.contains(&block.id) {
            continue;
        }
        for phi in &block.phis {
            let mut kind = if phi
                .place
                .identifier
                .name
                .as_ref()
                .is_some_and(|n| Environment::is_hook_name(n.value()))
            {
                Kind::PotentialHook
            } else {
                Kind::Local
            };
            for operand in phi.operands.values() {
                if let Some(&operand_kind) = value_kinds.get(&operand.identifier.id) {
                    kind = join_kinds(kind, operand_kind);
                }
            }
            value_kinds.insert(phi.place.identifier.id, kind);
        }

        for instr in &block.instructions {
            if let InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } = &instr.value
            {
                id_string_values.insert(instr.lvalue.identifier.id, s.clone());
            }
            match &instr.value {
                InstructionValue::ObjectMethod { lowered_func, .. }
                | InstructionValue::FunctionExpression { lowered_func, .. } => {
                    visit_function_expression(diagnostics, &lowered_func.func);
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    if is_known_hook_load_global(binding, &instr.lvalue) {
                        value_kinds.insert(instr.lvalue.identifier.id, Kind::KnownHook);
                    } else {
                        value_kinds.insert(instr.lvalue.identifier.id, Kind::Global);
                    }
                }
                InstructionValue::LoadContext { place, .. }
                | InstructionValue::LoadLocal { place, .. } => {
                    let kind = get_kind_for_place(&value_kinds, place);
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    let kind = join_kinds(
                        get_kind_for_place(&value_kinds, value),
                        get_kind_for_place(&value_kinds, &lvalue.place),
                    );
                    value_kinds.insert(lvalue.place.identifier.id, kind);
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                }
                InstructionValue::ComputedLoad { object, .. } => {
                    let kind = get_kind_for_place(&value_kinds, object);
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, join_kinds(lvalue_kind, kind));
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    let object_kind = get_kind_for_place(&value_kinds, object);
                    let is_hook_property = match property {
                        PropertyLiteral::String(s) => Environment::is_hook_name(s),
                        PropertyLiteral::Number(_) => false,
                    };
                    let kind = match object_kind {
                        Kind::Error => Kind::Error,
                        Kind::KnownHook => {
                            if is_hook_property {
                                Kind::KnownHook
                            } else {
                                Kind::Local
                            }
                        }
                        Kind::PotentialHook => Kind::PotentialHook,
                        Kind::Global => {
                            if is_hook_property {
                                Kind::KnownHook
                            } else {
                                Kind::Global
                            }
                        }
                        Kind::Local => {
                            if is_hook_property {
                                Kind::PotentialHook
                            } else {
                                Kind::Local
                            }
                        }
                    };
                    value_kinds.insert(instr.lvalue.identifier.id, kind);
                }
                InstructionValue::MethodCall {
                    receiver, property, ..
                } => {
                    let callee_kind = resolve_method_callee_kind(
                        &value_kinds,
                        &id_string_values,
                        receiver,
                        property,
                    );
                    if callee_kind == Kind::KnownHook || callee_kind == Kind::PotentialHook {
                        let hook_desc = hook_desc_for_place(property);
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: format!(
                                "Hooks must be called at the top level in the body of a function \
                                 component or custom hook, and may not be called within function \
                                 expressions. See the Rules of Hooks \
                                 (https://react.dev/warnings/invalid-hook-call-warning). \
                                 Cannot call {} within a function expression.",
                                hook_desc
                            ),
                        });
                    }
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::CallExpression { callee, .. } => {
                    let callee_kind = get_kind_for_place(&value_kinds, callee);
                    if callee_kind == Kind::KnownHook || callee_kind == Kind::PotentialHook {
                        let hook_desc = hook_desc_for_place(callee);
                        diagnostics.push(CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: format!(
                                "Hooks must be called at the top level in the body of a function \
                                 component or custom hook, and may not be called within function \
                                 expressions. See the Rules of Hooks \
                                 (https://react.dev/warnings/invalid-hook-call-warning). \
                                 Cannot call {} within a function expression.",
                                hook_desc
                            ),
                        });
                    }
                    let lvalue_kind = get_kind_for_place(&value_kinds, &instr.lvalue);
                    value_kinds.insert(instr.lvalue.identifier.id, lvalue_kind);
                }
                InstructionValue::Destructure { value, .. } => {
                    let object_kind = get_kind_for_place(&value_kinds, value);
                    for_each_instruction_lvalue(instr, |lvalue| {
                        let is_hook_property = lvalue
                            .identifier
                            .name
                            .as_ref()
                            .is_some_and(|n| Environment::is_hook_name(n.value()));
                        let kind = match object_kind {
                            Kind::Error => Kind::Error,
                            Kind::KnownHook => Kind::KnownHook,
                            Kind::PotentialHook => Kind::PotentialHook,
                            Kind::Global => {
                                if is_hook_property {
                                    Kind::KnownHook
                                } else {
                                    Kind::Global
                                }
                            }
                            Kind::Local => {
                                if is_hook_property {
                                    Kind::PotentialHook
                                } else {
                                    Kind::Local
                                }
                            }
                        };
                        value_kinds.insert(lvalue.identifier.id, kind);
                    });
                }
                _ => {
                    for_each_instruction_lvalue(instr, |lvalue| {
                        let kind = get_kind_for_place(&value_kinds, lvalue);
                        value_kinds.insert(lvalue.identifier.id, kind);
                    });
                }
            }
        }
    }
}

/// Compute the set of blocks that are unconditionally executed from the entry block
/// using a post-dominator tree.
///
/// A block is unconditional if it post-dominates the entry block — i.e., all paths
/// from entry to function exit must pass through it. This correctly handles early
/// returns, loops that may not execute, and conditional exits.
fn compute_unconditional_blocks(func: &HIRFunction) -> HashSet<BlockId> {
    use crate::hir::dominator::{PostDominatorOptions, compute_post_dominator_tree};

    let post_doms = compute_post_dominator_tree(
        func,
        PostDominatorOptions {
            include_throws_as_exit_node: false,
        },
    );

    let mut unconditional = HashSet::new();
    let mut current = Some(func.body.entry);
    let exit = post_doms.exit();

    while let Some(block_id) = current {
        if block_id == exit {
            break;
        }
        unconditional.insert(block_id);
        current = post_doms.get(block_id);
    }

    unconditional
}

fn compute_reachable_blocks(func: &HIRFunction) -> HashSet<BlockId> {
    use crate::hir::builder::terminal_successors;

    let mut reachable = HashSet::new();
    let mut worklist = vec![func.body.entry];

    while let Some(block_id) = worklist.pop() {
        if !reachable.insert(block_id) {
            continue;
        }
        if let Some((_, block)) = func.body.blocks.iter().find(|(id, _)| *id == block_id) {
            for succ in terminal_successors(&block.terminal) {
                worklist.push(succ);
            }
        }
    }

    reachable
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
                preds: std::collections::HashSet::new(),
                phis: vec![],
            },
        )
    }

    #[test]
    fn test_no_hooks_is_ok() {
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
        assert!(validate_hooks_usage(&func).is_ok());
    }

    #[test]
    fn test_unconditional_hook_call_is_ok() {
        // Block 0: LoadGlobal("useState") -> $1, CallExpression($1) -> $2, Return $2
        let block = make_basic_block_with_terminal(
            0,
            vec![
                Instruction {
                    id: InstructionId(0),
                    lvalue: make_test_place(1, Some("useState")),
                    value: InstructionValue::LoadGlobal {
                        binding: NonLocalBinding::Global {
                            name: "useState".to_string(),
                        },
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                },
                Instruction {
                    id: InstructionId(1),
                    lvalue: make_test_place(2, None),
                    value: InstructionValue::CallExpression {
                        callee: make_test_place(1, Some("useState")),
                        args: vec![],
                        optional: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                    effects: None,
                },
            ],
            Terminal::Return {
                value: make_test_place(2, None),
                return_variant: ReturnVariant::Explicit,
                id: InstructionId(2),
                loc: SourceLocation::Generated,
            },
        );
        let func = make_hir_function(vec![block]);
        assert!(validate_hooks_usage(&func).is_ok());
    }

    #[test]
    fn test_join_kinds_lattice() {
        assert_eq!(join_kinds(Kind::Error, Kind::Local), Kind::Error);
        assert_eq!(join_kinds(Kind::Local, Kind::Error), Kind::Error);
        assert_eq!(join_kinds(Kind::KnownHook, Kind::Local), Kind::KnownHook);
        assert_eq!(
            join_kinds(Kind::PotentialHook, Kind::Local),
            Kind::PotentialHook
        );
        assert_eq!(join_kinds(Kind::Global, Kind::Local), Kind::Global);
        assert_eq!(join_kinds(Kind::Local, Kind::Local), Kind::Local);
    }
}
