//! Validates that refs (from useRef) aren't accessed during render.
//!
//! Port of `ValidateNoRefAccessInRender.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This validates that a function does not access a ref value during render.
//! This includes a partial check for ref values which are accessed indirectly
//! via function expressions.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::*;
use crate::hir::visitors::{
    for_each_instruction_operand, for_each_pattern_place, for_each_terminal_operand,
};

// ---------------------------------------------------------------------------
// RefId — opaque identity for tracking individual refs
// ---------------------------------------------------------------------------

type RefId = u32;

struct RefIdGen {
    next: u32,
}

impl RefIdGen {
    fn new() -> Self {
        Self { next: 0 }
    }
    fn next(&mut self) -> RefId {
        let id = self.next;
        self.next += 1;
        id
    }
}

// ---------------------------------------------------------------------------
// RefAccessType — data-flow type for tracking ref-related values
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum RefAccessType {
    None,
    Nullable,
    Guard {
        ref_id: RefId,
    },
    Ref {
        ref_id: RefId,
    },
    RefValue {
        loc: Option<SourceLocation>,
        ref_id: Option<RefId>,
    },
    Structure {
        value: Option<Box<RefAccessType>>,
        fn_type: Option<Box<RefFnType>>,
    },
}

#[derive(Debug, Clone)]
struct RefFnType {
    read_ref_effect: bool,
    return_type: RefAccessType,
}

// ---------------------------------------------------------------------------
// Type equality (for convergence check)
// ---------------------------------------------------------------------------

fn ty_equal(a: &RefAccessType, b: &RefAccessType) -> bool {
    match (a, b) {
        (RefAccessType::None, RefAccessType::None) => true,
        (RefAccessType::Nullable, RefAccessType::Nullable) => true,
        (RefAccessType::Ref { .. }, RefAccessType::Ref { .. }) => true,
        (RefAccessType::Guard { ref_id: a_id }, RefAccessType::Guard { ref_id: b_id }) => {
            a_id == b_id
        }
        (
            RefAccessType::RefValue { loc: a_loc, .. },
            RefAccessType::RefValue { loc: b_loc, .. },
        ) => match (a_loc, b_loc) {
            (Option::None, Option::None) => true,
            (Some(a), Some(b)) => std::mem::discriminant(a) == std::mem::discriminant(b),
            _ => false,
        },
        (
            RefAccessType::Structure {
                value: a_val,
                fn_type: a_fn,
            },
            RefAccessType::Structure {
                value: b_val,
                fn_type: b_fn,
            },
        ) => {
            let fn_eq = match (a_fn.as_deref(), b_fn.as_deref()) {
                (Option::None, Option::None) => true,
                (Some(af), Some(bf)) => {
                    af.read_ref_effect == bf.read_ref_effect
                        && ty_equal(&af.return_type, &bf.return_type)
                }
                _ => false,
            };
            let val_eq = match (a_val.as_deref(), b_val.as_deref()) {
                (Option::None, Option::None) => true,
                (Some(av), Some(bv)) => ty_equal(av, bv),
                _ => false,
            };
            fn_eq && val_eq
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Join (widening) — merges two RefAccessTypes for convergence
// ---------------------------------------------------------------------------

fn join_ref_access_ref_types(
    a: &RefAccessType,
    b: &RefAccessType,
    id_gen: &mut RefIdGen,
) -> RefAccessType {
    match (a, b) {
        (
            RefAccessType::RefValue { ref_id: a_id, .. },
            RefAccessType::RefValue { ref_id: b_id, .. },
        ) => {
            if a_id == b_id {
                a.clone()
            } else {
                RefAccessType::RefValue {
                    loc: Option::None,
                    ref_id: Option::None,
                }
            }
        }
        (RefAccessType::RefValue { .. }, _) => a.clone(),
        (_, RefAccessType::RefValue { .. }) => b.clone(),
        (RefAccessType::Ref { ref_id: a_id }, RefAccessType::Ref { ref_id: b_id }) => {
            if a_id == b_id {
                a.clone()
            } else {
                RefAccessType::Ref {
                    ref_id: id_gen.next(),
                }
            }
        }
        (RefAccessType::Ref { .. }, _) | (_, RefAccessType::Ref { .. }) => RefAccessType::Ref {
            ref_id: id_gen.next(),
        },
        (
            RefAccessType::Structure {
                value: a_val,
                fn_type: a_fn,
            },
            RefAccessType::Structure {
                value: b_val,
                fn_type: b_fn,
            },
        ) => {
            let fn_type = match (a_fn.as_deref(), b_fn.as_deref()) {
                (Option::None, other) | (other, Option::None) => other.map(|f| Box::new(f.clone())),
                (Some(af), Some(bf)) => Some(Box::new(RefFnType {
                    read_ref_effect: af.read_ref_effect || bf.read_ref_effect,
                    return_type: join_ref_access_types_pair(
                        &af.return_type,
                        &bf.return_type,
                        id_gen,
                    ),
                })),
            };
            let value = match (a_val.as_deref(), b_val.as_deref()) {
                (Option::None, other) | (other, Option::None) => other.map(|v| Box::new(v.clone())),
                (Some(av), Some(bv)) => Some(Box::new(join_ref_access_ref_types(av, bv, id_gen))),
            };
            RefAccessType::Structure { value, fn_type }
        }
        // Shouldn't reach here with well-typed inputs
        _ => a.clone(),
    }
}

fn join_ref_access_types_pair(
    a: &RefAccessType,
    b: &RefAccessType,
    id_gen: &mut RefIdGen,
) -> RefAccessType {
    match (a, b) {
        (RefAccessType::None, other) | (other, RefAccessType::None) => other.clone(),
        (RefAccessType::Guard { ref_id: a_id }, RefAccessType::Guard { ref_id: b_id }) => {
            if a_id == b_id {
                a.clone()
            } else {
                RefAccessType::None
            }
        }
        (RefAccessType::Guard { .. }, RefAccessType::Nullable)
        | (RefAccessType::Nullable, RefAccessType::Guard { .. }) => RefAccessType::None,
        (RefAccessType::Guard { .. }, other) | (other, RefAccessType::Guard { .. }) => {
            other.clone()
        }
        (RefAccessType::Nullable, other) | (other, RefAccessType::Nullable) => other.clone(),
        _ => join_ref_access_ref_types(a, b, id_gen),
    }
}

fn join_ref_access_types(types: &[RefAccessType], id_gen: &mut RefIdGen) -> RefAccessType {
    types.iter().fold(RefAccessType::None, |acc, t| {
        join_ref_access_types_pair(&acc, t, id_gen)
    })
}

// ---------------------------------------------------------------------------
// Helper — check if identifier type is a useRef type
// ---------------------------------------------------------------------------

fn is_use_ref_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInUseRefId")
}

fn is_ref_value_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInRefValue")
}

fn ref_type_of_type(place: &Place, id_gen: &mut RefIdGen) -> RefAccessType {
    if is_ref_value_type(&place.identifier) {
        RefAccessType::RefValue {
            loc: Option::None,
            ref_id: Option::None,
        }
    } else if is_use_ref_type(&place.identifier) {
        RefAccessType::Ref {
            ref_id: id_gen.next(),
        }
    } else {
        RefAccessType::None
    }
}

/// Build hook-name lookup for lowered callees.
///
/// Hook callees are often loaded through temporaries (LoadGlobal/LoadLocal) or,
/// for method calls, through Primitive string properties ("useEffect").
fn build_hook_name_lookup(func: &HIRFunction) -> HashMap<IdentifierId, String> {
    let mut hook_names: HashMap<IdentifierId, String> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(name) = &instr.lvalue.identifier.name {
                let name = name.value().to_string();
                if Environment::is_hook_name(&name) {
                    hook_names.insert(instr.lvalue.identifier.id, name);
                }
            }

            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name().to_string();
                    if Environment::is_hook_name(&name) {
                        hook_names.insert(instr.lvalue.identifier.id, name);
                    }
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(name) = hook_names.get(&place.identifier.id) {
                        hook_names.insert(instr.lvalue.identifier.id, name.clone());
                    }
                    if let Some(name) = &place.identifier.name {
                        let name = name.value().to_string();
                        if Environment::is_hook_name(&name) {
                            hook_names.insert(instr.lvalue.identifier.id, name);
                        }
                    }
                }
                InstructionValue::TypeCastExpression { value, .. } => {
                    if let Some(name) = hook_names.get(&value.identifier.id) {
                        hook_names.insert(instr.lvalue.identifier.id, name.clone());
                    }
                }
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(name),
                    ..
                } => {
                    if Environment::is_hook_name(name) {
                        hook_names.insert(instr.lvalue.identifier.id, name.clone());
                    }
                }
                _ => {}
            }
        }
    }

    hook_names
}

/// Build a generic callee-name lookup for lowered temporaries.
fn build_callee_name_lookup(func: &HIRFunction) -> HashMap<IdentifierId, String> {
    let mut names: HashMap<IdentifierId, String> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(name) = &instr.lvalue.identifier.name {
                names.insert(instr.lvalue.identifier.id, name.value().to_string());
            }

            match &instr.value {
                InstructionValue::LoadGlobal { binding, .. } => {
                    names.insert(instr.lvalue.identifier.id, binding.name().to_string());
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(name) = names.get(&place.identifier.id) {
                        names.insert(instr.lvalue.identifier.id, name.clone());
                    } else if let Some(name) = &place.identifier.name {
                        names.insert(instr.lvalue.identifier.id, name.value().to_string());
                    }
                }
                InstructionValue::TypeCastExpression { value, .. } => {
                    if let Some(name) = names.get(&value.identifier.id) {
                        names.insert(instr.lvalue.identifier.id, name.clone());
                    }
                }
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(name),
                    ..
                } => {
                    names.insert(instr.lvalue.identifier.id, name.clone());
                }
                _ => {}
            }
        }
    }

    names
}

fn get_callee_name(callee: &Place, names: &HashMap<IdentifierId, String>) -> Option<String> {
    if let Some(name) = &callee.identifier.name {
        return Some(name.value().to_string());
    }
    names.get(&callee.identifier.id).cloned()
}

/// Approximate hook kind check. Returns Some("useXxx") if the callee appears
/// to be a hook call, None otherwise.
fn get_hook_kind_for_callee(
    callee: &Place,
    hook_names: &HashMap<IdentifierId, String>,
) -> Option<String> {
    if let Some(name) = &callee.identifier.name {
        let n = name.value();
        if Environment::is_hook_name(n) {
            return Some(n.to_string());
        }
    }
    hook_names.get(&callee.identifier.id).cloned()
}

// ---------------------------------------------------------------------------
// Env — data-flow environment
// ---------------------------------------------------------------------------

struct RefEnv {
    data: HashMap<IdentifierId, RefAccessType>,
    temporaries: HashMap<IdentifierId, IdentifierId>,
    changed: bool,
}

impl RefEnv {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
            temporaries: HashMap::new(),
            changed: false,
        }
    }

    fn lookup(&self, id: IdentifierId) -> IdentifierId {
        self.temporaries.get(&id).copied().unwrap_or(id)
    }

    fn define(&mut self, from: IdentifierId, to: IdentifierId) {
        self.temporaries.insert(from, to);
    }

    fn get(&self, key: IdentifierId) -> Option<&RefAccessType> {
        let operand_id = self.lookup(key);
        self.data.get(&operand_id)
    }

    fn set(&mut self, key: IdentifierId, value: RefAccessType, id_gen: &mut RefIdGen) {
        let operand_id = self.lookup(key);
        let cur = self.data.get(&operand_id);
        let widened = match cur {
            Some(c) => join_ref_access_types_pair(&value, c, id_gen),
            None => join_ref_access_types_pair(&value, &RefAccessType::None, id_gen),
        };
        let widened_none = matches!(&widened, RefAccessType::None);
        if !(cur.is_none() && widened_none) && (cur.is_none() || !ty_equal(cur.unwrap(), &widened))
        {
            self.changed = true;
        }
        self.data.insert(operand_id, widened);
    }

    fn reset_changed(&mut self) {
        self.changed = false;
    }

    fn has_changed(&self) -> bool {
        self.changed
    }
}

// ---------------------------------------------------------------------------
// Destructure helper — unwrap Structure.value recursively
// ---------------------------------------------------------------------------

fn destructure_type(ty: Option<&RefAccessType>) -> Option<&RefAccessType> {
    match ty {
        Some(RefAccessType::Structure {
            value: Some(inner), ..
        }) => destructure_type(Some(inner.as_ref())),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn validate_no_direct_ref_value_access(
    diagnostics: &mut Vec<CompilerDiagnostic>,
    operand: &Place,
    env: &RefEnv,
) {
    let ty = destructure_type(env.get(operand.identifier.id));
    if let Some(RefAccessType::RefValue { .. }) = ty {
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message:
                "Cannot access ref value during render. (https://react.dev/reference/react/useRef)"
                    .to_string(),
            category: Some(ErrorCategory::Refs),
        });
    }
}

fn validate_no_ref_value_access(
    diagnostics: &mut Vec<CompilerDiagnostic>,
    operand: &Place,
    env: &RefEnv,
) {
    let ty = destructure_type(env.get(operand.identifier.id));
    match ty {
        Some(RefAccessType::RefValue { .. }) => {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: "Cannot access ref value during render. (https://react.dev/reference/react/useRef)".to_string(),
                category: Some(ErrorCategory::Refs),
            });
        }
        Some(RefAccessType::Structure {
            fn_type: Some(f), ..
        }) if f.read_ref_effect => {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: "Cannot access ref value during render. (https://react.dev/reference/react/useRef)".to_string(),
                category: Some(ErrorCategory::Refs),
            });
        }
        _ => {}
    }
}

fn validate_no_ref_passed_to_function(
    diagnostics: &mut Vec<CompilerDiagnostic>,
    operand: &Place,
    env: &RefEnv,
) {
    let ty = destructure_type(env.get(operand.identifier.id));
    match ty {
        Some(RefAccessType::Ref { .. }) | Some(RefAccessType::RefValue { .. }) => {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message:
                    "Ref values (the `current` property) may not be accessed during render. (https://react.dev/reference/react/useRef)"
                        .to_string(),
                category: Some(ErrorCategory::Refs),
            });
        }
        Some(RefAccessType::Structure {
            fn_type: Some(f), ..
        }) if f.read_ref_effect => {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message:
                    "Ref values (the `current` property) may not be accessed during render. (https://react.dev/reference/react/useRef)"
                        .to_string(),
                category: Some(ErrorCategory::Refs),
            });
        }
        _ => {}
    }
}

fn validate_no_ref_update(
    diagnostics: &mut Vec<CompilerDiagnostic>,
    operand: &Place,
    env: &RefEnv,
) {
    let ty = destructure_type(env.get(operand.identifier.id));
    match ty {
        Some(RefAccessType::Ref { .. }) | Some(RefAccessType::RefValue { .. }) => {
            diagnostics.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: "Ref values (the `current` property) may not be accessed during render. (https://react.dev/reference/react/useRef)".to_string(),
                category: Some(ErrorCategory::Refs),
            });
        }
        _ => {}
    }
}

fn guard_check(diagnostics: &mut Vec<CompilerDiagnostic>, operand: &Place, env: &RefEnv) {
    if let Some(RefAccessType::Guard { .. }) = env.get(operand.identifier.id) {
        diagnostics.push(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message:
                "Cannot access ref value during render. (https://react.dev/reference/react/useRef)"
                    .to_string(),
            category: Some(ErrorCategory::Refs),
        });
    }
}

// ---------------------------------------------------------------------------
// Collect temporaries sidemap
// ---------------------------------------------------------------------------

fn collect_temporaries_sidemap(func: &HIRFunction, env: &mut RefEnv) {
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. } => {
                    let temp = env.lookup(place.identifier.id);
                    env.define(instr.lvalue.identifier.id, temp);
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let temp = env.lookup(value.identifier.id);
                    env.define(instr.lvalue.identifier.id, temp);
                    env.define(lvalue.place.identifier.id, temp);
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    // Skip ref.current access — don't alias through it
                    if is_use_ref_type(&object.identifier)
                        && let PropertyLiteral::String(s) = property
                        && s == "current"
                    {
                        continue;
                    }
                    let temp = env.lookup(object.identifier.id);
                    env.define(instr.lvalue.identifier.id, temp);
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Main validation implementation
// ---------------------------------------------------------------------------

/// Internal implementation that returns the joined return type.
fn validate_impl(
    func: &HIRFunction,
    env: &mut RefEnv,
    id_gen: &mut RefIdGen,
) -> Result<RefAccessType, Vec<CompilerDiagnostic>> {
    let debug_ref_access = std::env::var("DEBUG_REF_ACCESS").is_ok();
    let hook_name_lookup = build_hook_name_lookup(func);
    let callee_name_lookup = build_callee_name_lookup(func);
    let mut return_values: Vec<RefAccessType> = Vec::new();

    // Initialize params
    for param in &func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        let ty = ref_type_of_type(place, id_gen);
        env.set(place.identifier.id, ty, id_gen);
    }

    // Collect JSX-interpolated identifiers
    let mut interpolated_as_jsx = HashSet::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::JsxExpression {
                    children: Some(children),
                    ..
                } => {
                    for child in children {
                        interpolated_as_jsx.insert(child.identifier.id);
                    }
                }
                InstructionValue::JsxFragment { children, .. } => {
                    for child in children {
                        interpolated_as_jsx.insert(child.identifier.id);
                    }
                }
                _ => {}
            }
        }
    }

    // Fixed-point iteration (max 10 rounds)
    for i in 0..10 {
        if i > 0 && !env.has_changed() {
            break;
        }
        env.reset_changed();
        return_values.clear();

        let mut safe_blocks: Vec<(BlockId, RefId)> = Vec::new();
        let mut diagnostics: Vec<CompilerDiagnostic> = Vec::new();

        for (_block_id, block) in &func.body.blocks {
            // Remove safe blocks for current block
            safe_blocks.retain(|(bid, _)| *bid != block.id);

            // Process phis
            for phi in &block.phis {
                let operand_types: Vec<RefAccessType> = phi
                    .operands
                    .values()
                    .map(|operand| {
                        env.get(operand.identifier.id)
                            .cloned()
                            .unwrap_or(RefAccessType::None)
                    })
                    .collect();
                let joined = join_ref_access_types(&operand_types, id_gen);
                env.set(phi.place.identifier.id, joined, id_gen);
            }

            // Process instructions
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. } => {
                        for_each_instruction_operand(instr, |operand| {
                            validate_no_direct_ref_value_access(&mut diagnostics, operand, env);
                        });
                    }

                    InstructionValue::ComputedLoad {
                        object, property, ..
                    } => {
                        validate_no_direct_ref_value_access(&mut diagnostics, property, env);
                        let obj_type = env.get(object.identifier.id).cloned();
                        let lookup_type = match &obj_type {
                            Some(RefAccessType::Structure { value: Some(v), .. }) => {
                                Some(v.as_ref().clone())
                            }
                            Some(RefAccessType::Ref { ref_id }) => Some(RefAccessType::RefValue {
                                loc: Some(instr.loc.clone()),
                                ref_id: Some(*ref_id),
                            }),
                            _ => None,
                        };
                        env.set(
                            instr.lvalue.identifier.id,
                            lookup_type.unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen)),
                            id_gen,
                        );
                    }

                    InstructionValue::PropertyLoad { object, .. } => {
                        let obj_type = env.get(object.identifier.id).cloned();
                        let lookup_type = match &obj_type {
                            Some(RefAccessType::Structure { value: Some(v), .. }) => {
                                Some(v.as_ref().clone())
                            }
                            Some(RefAccessType::Ref { ref_id }) => Some(RefAccessType::RefValue {
                                loc: Some(instr.loc.clone()),
                                ref_id: Some(*ref_id),
                            }),
                            _ => None,
                        };
                        env.set(
                            instr.lvalue.identifier.id,
                            lookup_type.unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen)),
                            id_gen,
                        );
                    }

                    InstructionValue::TypeCastExpression { value, .. } => {
                        let ty = env
                            .get(value.identifier.id)
                            .cloned()
                            .unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen));
                        env.set(instr.lvalue.identifier.id, ty, id_gen);
                    }

                    InstructionValue::LoadContext { place, .. }
                    | InstructionValue::LoadLocal { place, .. } => {
                        let ty = env
                            .get(place.identifier.id)
                            .cloned()
                            .unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen));
                        env.set(instr.lvalue.identifier.id, ty, id_gen);
                    }

                    InstructionValue::StoreContext { lvalue, value, .. }
                    | InstructionValue::StoreLocal { lvalue, value, .. } => {
                        let ty = env
                            .get(value.identifier.id)
                            .cloned()
                            .unwrap_or_else(|| ref_type_of_type(&lvalue.place, id_gen));
                        env.set(lvalue.place.identifier.id, ty.clone(), id_gen);
                        let ty2 = env
                            .get(value.identifier.id)
                            .cloned()
                            .unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen));
                        env.set(instr.lvalue.identifier.id, ty2, id_gen);
                    }

                    InstructionValue::Destructure { lvalue, value, .. } => {
                        let obj_type = env.get(value.identifier.id).cloned();
                        let lookup_type = match &obj_type {
                            Some(RefAccessType::Structure { value: Some(v), .. }) => {
                                Some(v.as_ref().clone())
                            }
                            _ => None,
                        };
                        env.set(
                            instr.lvalue.identifier.id,
                            lookup_type
                                .clone()
                                .unwrap_or_else(|| ref_type_of_type(&instr.lvalue, id_gen)),
                            id_gen,
                        );
                        for_each_pattern_place(&lvalue.pattern, &mut |pat_place| {
                            env.set(
                                pat_place.identifier.id,
                                lookup_type
                                    .clone()
                                    .unwrap_or_else(|| ref_type_of_type(pat_place, id_gen)),
                                id_gen,
                            );
                        });
                    }

                    InstructionValue::ObjectMethod { lowered_func, .. }
                    | InstructionValue::FunctionExpression { lowered_func, .. } => {
                        let mut inner_env = RefEnv::new();
                        // Share temporaries from outer env
                        inner_env.temporaries = env.temporaries.clone();
                        inner_env.data = env.data.clone();
                        collect_temporaries_sidemap(&lowered_func.func, &mut inner_env);

                        let mut return_type = RefAccessType::None;
                        let mut read_ref_effect = false;

                        match validate_impl(&lowered_func.func, &mut inner_env, id_gen) {
                            Ok(rt) => {
                                return_type = rt;
                            }
                            Err(_) => {
                                read_ref_effect = true;
                            }
                        }

                        env.set(
                            instr.lvalue.identifier.id,
                            RefAccessType::Structure {
                                fn_type: Some(Box::new(RefFnType {
                                    read_ref_effect,
                                    return_type,
                                })),
                                value: None,
                            },
                            id_gen,
                        );
                    }

                    InstructionValue::MethodCall { property, .. } => {
                        let callee = property;
                        let hook_kind = get_hook_kind_for_callee(callee, &hook_name_lookup);
                        let callee_name = get_callee_name(callee, &callee_name_lookup);
                        let is_merge_refs_call =
                            matches!(callee_name.as_deref(), Some("mergeRefs"));
                        let mut return_type = RefAccessType::None;
                        let fn_type = env.get(callee.identifier.id).cloned();
                        let mut did_error = false;

                        if let Some(RefAccessType::Structure {
                            fn_type: Some(f), ..
                        }) = &fn_type
                        {
                            return_type = f.return_type.clone();
                            if f.read_ref_effect {
                                did_error = true;
                                diagnostics.push(CompilerDiagnostic {
                                    severity: DiagnosticSeverity::InvalidReact,
                                    message: "This function accesses a ref value. (https://react.dev/reference/react/useRef)".to_string(),
                                    category: Some(ErrorCategory::Refs),
                                });
                            }
                        }

                        if !did_error {
                            let is_ref_lvalue = is_use_ref_type(&instr.lvalue.identifier);
                            if debug_ref_access {
                                eprintln!(
                                    "[REF_ACCESS] method-call callee={:?} hook_kind={:?} is_ref_lvalue={} merge_refs={}",
                                    callee_name, hook_kind, is_ref_lvalue, is_merge_refs_call
                                );
                            }
                            for_each_instruction_operand(instr, |operand| {
                                if is_ref_lvalue
                                    || is_merge_refs_call
                                    || (hook_kind.is_some()
                                        && hook_kind.as_deref() != Some("useState")
                                        && hook_kind.as_deref() != Some("useReducer"))
                                {
                                    validate_no_direct_ref_value_access(
                                        &mut diagnostics,
                                        operand,
                                        env,
                                    );
                                } else if interpolated_as_jsx.contains(&instr.lvalue.identifier.id)
                                {
                                    validate_no_ref_value_access(&mut diagnostics, operand, env);
                                } else {
                                    validate_no_ref_passed_to_function(
                                        &mut diagnostics,
                                        operand,
                                        env,
                                    );
                                }
                            });
                        }
                        env.set(instr.lvalue.identifier.id, return_type, id_gen);
                    }

                    InstructionValue::CallExpression { callee, .. } => {
                        let hook_kind = get_hook_kind_for_callee(callee, &hook_name_lookup);
                        let callee_name = get_callee_name(callee, &callee_name_lookup);
                        let is_merge_refs_call =
                            matches!(callee_name.as_deref(), Some("mergeRefs"));
                        let mut return_type = RefAccessType::None;
                        let fn_type = env.get(callee.identifier.id).cloned();
                        let mut did_error = false;

                        if let Some(RefAccessType::Structure {
                            fn_type: Some(f), ..
                        }) = &fn_type
                        {
                            return_type = f.return_type.clone();
                            if f.read_ref_effect {
                                did_error = true;
                                diagnostics.push(CompilerDiagnostic {
                                    severity: DiagnosticSeverity::InvalidReact,
                                    message: "This function accesses a ref value. (https://react.dev/reference/react/useRef)".to_string(),
                                    category: Some(ErrorCategory::Refs),
                                });
                            }
                        }

                        if !did_error {
                            let is_ref_lvalue = is_use_ref_type(&instr.lvalue.identifier);
                            if debug_ref_access {
                                eprintln!(
                                    "[REF_ACCESS] call callee={:?} hook_kind={:?} is_ref_lvalue={} merge_refs={}",
                                    callee_name, hook_kind, is_ref_lvalue, is_merge_refs_call
                                );
                            }
                            for_each_instruction_operand(instr, |operand| {
                                if is_ref_lvalue
                                    || is_merge_refs_call
                                    || (hook_kind.is_some()
                                        && hook_kind.as_deref() != Some("useState")
                                        && hook_kind.as_deref() != Some("useReducer"))
                                {
                                    validate_no_direct_ref_value_access(
                                        &mut diagnostics,
                                        operand,
                                        env,
                                    );
                                } else if interpolated_as_jsx.contains(&instr.lvalue.identifier.id)
                                {
                                    validate_no_ref_value_access(&mut diagnostics, operand, env);
                                } else {
                                    validate_no_ref_passed_to_function(
                                        &mut diagnostics,
                                        operand,
                                        env,
                                    );
                                }
                            });
                        }
                        env.set(instr.lvalue.identifier.id, return_type, id_gen);
                    }

                    InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. } => {
                        let mut types = Vec::new();
                        for_each_instruction_operand(instr, |operand| {
                            validate_no_direct_ref_value_access(&mut diagnostics, operand, env);
                            types.push(
                                env.get(operand.identifier.id)
                                    .cloned()
                                    .unwrap_or(RefAccessType::None),
                            );
                        });
                        let value = join_ref_access_types(&types, id_gen);
                        match &value {
                            RefAccessType::None
                            | RefAccessType::Guard { .. }
                            | RefAccessType::Nullable => {
                                env.set(instr.lvalue.identifier.id, RefAccessType::None, id_gen);
                            }
                            _ => {
                                env.set(
                                    instr.lvalue.identifier.id,
                                    RefAccessType::Structure {
                                        value: Some(Box::new(value)),
                                        fn_type: None,
                                    },
                                    id_gen,
                                );
                            }
                        }
                    }

                    InstructionValue::PropertyDelete { object, .. }
                    | InstructionValue::PropertyStore { object, .. }
                    | InstructionValue::ComputedDelete { object, .. }
                    | InstructionValue::ComputedStore { object, .. } => {
                        let target = env.get(object.identifier.id).cloned();
                        let mut safe_found = false;

                        if matches!(&instr.value, InstructionValue::PropertyStore { .. })
                            && let Some(RefAccessType::Ref { ref_id }) = &target
                        {
                            if let Some(pos) = safe_blocks.iter().position(|(_, rid)| rid == ref_id)
                            {
                                safe_blocks.remove(pos);
                                safe_found = true;
                            }
                            if debug_ref_access {
                                eprintln!(
                                    "[REF_ACCESS] property-store block={} object={} ref_id={} safe_found={} safe_blocks={:?}",
                                    block.id.0,
                                    object.identifier.id.0,
                                    ref_id,
                                    safe_found,
                                    safe_blocks
                                );
                            }
                        }

                        if !safe_found {
                            validate_no_ref_update(&mut diagnostics, object, env);
                        }

                        // Validate computed property operand
                        match &instr.value {
                            InstructionValue::ComputedDelete { property, .. }
                            | InstructionValue::ComputedStore { property, .. } => {
                                validate_no_ref_value_access(&mut diagnostics, property, env);
                            }
                            _ => {}
                        }

                        // Track structure propagation for stores
                        match &instr.value {
                            InstructionValue::ComputedStore { value, object, .. }
                            | InstructionValue::PropertyStore { value, object, .. } => {
                                validate_no_direct_ref_value_access(&mut diagnostics, value, env);
                                let val_type = env.get(value.identifier.id).cloned();
                                if let Some(RefAccessType::Structure { .. }) = &val_type {
                                    let mut object_type = val_type.unwrap();
                                    if let Some(t) = &target {
                                        object_type =
                                            join_ref_access_types_pair(&object_type, t, id_gen);
                                    }
                                    env.set(object.identifier.id, object_type, id_gen);
                                }
                            }
                            _ => {}
                        }
                    }

                    InstructionValue::StartMemoize { .. }
                    | InstructionValue::FinishMemoize { .. } => {}

                    InstructionValue::LoadGlobal { binding, .. } => {
                        if binding.name() == "undefined" {
                            env.set(instr.lvalue.identifier.id, RefAccessType::Nullable, id_gen);
                        }
                    }

                    InstructionValue::Primitive { value, .. } => {
                        if matches!(value, PrimitiveValue::Null | PrimitiveValue::Undefined) {
                            env.set(instr.lvalue.identifier.id, RefAccessType::Nullable, id_gen);
                        }
                    }

                    InstructionValue::UnaryExpression {
                        operator, value, ..
                    } if *operator == UnaryOperator::Not => {
                        let val_type = env.get(value.identifier.id).cloned();
                        let ref_id = match &val_type {
                            Some(RefAccessType::RefValue {
                                ref_id: Some(rid), ..
                            }) => Some(*rid),
                            _ => None,
                        };
                        if let Some(rid) = ref_id {
                            env.set(
                                instr.lvalue.identifier.id,
                                RefAccessType::Guard { ref_id: rid },
                                id_gen,
                            );
                            diagnostics.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::InvalidReact,
                                message: "Cannot access ref value during render. (https://react.dev/reference/react/useRef)".to_string(),
                                category: Some(ErrorCategory::Refs),
                            });
                        } else {
                            validate_no_ref_value_access(&mut diagnostics, value, env);
                        }
                    }

                    InstructionValue::BinaryExpression { left, right, .. } => {
                        let left_ty = env.get(left.identifier.id).cloned();
                        let right_ty = env.get(right.identifier.id).cloned();

                        let ref_id = match &left_ty {
                            Some(RefAccessType::RefValue {
                                ref_id: Some(rid), ..
                            }) => Some(*rid),
                            _ => match &right_ty {
                                Some(RefAccessType::RefValue {
                                    ref_id: Some(rid), ..
                                }) => Some(*rid),
                                _ => None,
                            },
                        };

                        let nullish = matches!(&left_ty, Some(RefAccessType::Nullable))
                            || matches!(&right_ty, Some(RefAccessType::Nullable));

                        if let Some(ref_id) = ref_id
                            && nullish
                        {
                            if debug_ref_access {
                                eprintln!(
                                    "[REF_ACCESS] guard-from-binary block={} lvalue={} ref_id={}",
                                    block.id.0, instr.lvalue.identifier.id.0, ref_id
                                );
                            }
                            env.set(
                                instr.lvalue.identifier.id,
                                RefAccessType::Guard { ref_id },
                                id_gen,
                            );
                        } else {
                            for_each_instruction_operand(instr, |operand| {
                                validate_no_ref_value_access(&mut diagnostics, operand, env);
                            });
                        }
                    }

                    // Default: validate all operands
                    _ => {
                        for_each_instruction_operand(instr, |operand| {
                            validate_no_ref_value_access(&mut diagnostics, operand, env);
                        });
                    }
                }

                // Guard values are derived from ref.current,
                // so they can only be used in if statement test positions
                for_each_instruction_operand(instr, |operand| {
                    guard_check(&mut diagnostics, operand, env);
                });

                // Ensure useRef-typed lvalues are tracked as Ref
                if is_use_ref_type(&instr.lvalue.identifier)
                    && !matches!(
                        env.get(instr.lvalue.identifier.id),
                        Some(RefAccessType::Ref { .. })
                    )
                {
                    let cur = env
                        .get(instr.lvalue.identifier.id)
                        .cloned()
                        .unwrap_or(RefAccessType::None);
                    let merged = join_ref_access_types_pair(
                        &cur,
                        &RefAccessType::Ref {
                            ref_id: id_gen.next(),
                        },
                        id_gen,
                    );
                    env.set(instr.lvalue.identifier.id, merged, id_gen);
                }

                // Ensure RefValue-typed lvalues are tracked as RefValue
                if is_ref_value_type(&instr.lvalue.identifier)
                    && !matches!(
                        env.get(instr.lvalue.identifier.id),
                        Some(RefAccessType::RefValue { .. })
                    )
                {
                    let cur = env
                        .get(instr.lvalue.identifier.id)
                        .cloned()
                        .unwrap_or(RefAccessType::None);
                    let merged = join_ref_access_types_pair(
                        &cur,
                        &RefAccessType::RefValue {
                            loc: Some(instr.loc.clone()),
                            ref_id: None,
                        },
                        id_gen,
                    );
                    env.set(instr.lvalue.identifier.id, merged, id_gen);
                }
            }

            // Terminal handling
            // If terminal with guard test → add fallthrough to safe blocks
            if let Terminal::If {
                test, fallthrough, ..
            } = &block.terminal
                && let Some(RefAccessType::Guard { ref_id }) = env.get(test.identifier.id)
                && !safe_blocks.iter().any(|(_, rid)| rid == ref_id)
            {
                safe_blocks.push((*fallthrough, *ref_id));
                if debug_ref_access {
                    eprintln!(
                        "[REF_ACCESS] add-safe-block from={} fallthrough={} ref_id={}",
                        block.id.0, fallthrough.0, ref_id
                    );
                }
            }

            // Validate terminal operands
            for_each_terminal_operand(&block.terminal, |operand| {
                if matches!(&block.terminal, Terminal::Return { .. }) {
                    // Allow functions containing refs to be returned, but not direct ref values
                    validate_no_direct_ref_value_access(&mut diagnostics, operand, env);
                    guard_check(&mut diagnostics, operand, env);
                    if let Some(ty) = env.get(operand.identifier.id) {
                        return_values.push(ty.clone());
                    }
                } else {
                    validate_no_ref_value_access(&mut diagnostics, operand, env);
                    if !matches!(&block.terminal, Terminal::If { .. }) {
                        guard_check(&mut diagnostics, operand, env);
                    }
                }
            });
        }

        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
    }

    Ok(join_ref_access_types(&return_values, id_gen))
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Validates that refs are not accessed during render.
pub fn validate_no_ref_access_in_render(func: &HIRFunction) -> Result<(), CompilerError> {
    let mut env = RefEnv::new();
    let mut id_gen = RefIdGen::new();

    collect_temporaries_sidemap(func, &mut env);

    match validate_impl(func, &mut env, &mut id_gen) {
        Ok(_) => Ok(()),
        Err(diagnostics) => Err(CompilerError::Bail(BailOut {
            reason: "Ref access in render".to_string(),
            diagnostics,
        })),
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

    fn make_ref_place(id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Object {
                    shape_id: Some("BuiltInUseRefId".to_string()),
                },
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_ref_value_place(id: u32) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: None,
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Object {
                    shape_id: Some("BuiltInRefValue".to_string()),
                },
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
        assert!(validate_no_ref_access_in_render(&func).is_ok());
    }

    #[test]
    fn test_ref_type_detection() {
        let ref_place = make_ref_place(1);
        assert!(is_use_ref_type(&ref_place.identifier));

        let normal_place = make_test_place(2);
        assert!(!is_use_ref_type(&normal_place.identifier));

        let ref_val_place = make_ref_value_place(3);
        assert!(is_ref_value_type(&ref_val_place.identifier));
    }
}
