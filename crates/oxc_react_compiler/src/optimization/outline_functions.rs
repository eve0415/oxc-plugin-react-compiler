//! Port of OutlineFunctions.ts from upstream React Compiler.
//!
//! Outlines anonymous function expressions that don't capture any context
//! variables. These are hoisted to top-level function declarations and the
//! original FunctionExpression is replaced with a LoadGlobal.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use crate::hir::types::*;
use std::collections::HashSet;

/// An outlined function ready for codegen as a top-level declaration.
#[derive(Debug)]
pub struct OutlinedFunction {
    pub name: String,
    pub func: HIRFunction,
}

/// Outline anonymous function expressions that capture no context.
///
/// Returns a list of outlined functions that should be emitted as top-level
/// declarations after the main function body.
pub fn outline_functions(
    func: &mut HIRFunction,
    fbt_operands: &HashSet<IdentifierId>,
) -> Vec<OutlinedFunction> {
    let mut outlined = Vec::new();
    let mut name_counter = 0u32;
    let mut used_names: HashSet<String> = HashSet::new();

    // Collect existing names to avoid collisions
    collect_used_names(func, &mut used_names);

    // Start with empty ancestor names (top-level function has no outer scope to capture from)
    let ancestor_names = HashSet::new();
    let ancestor_decls = HashSet::new();
    outline_functions_inner(
        func,
        &mut outlined,
        &mut name_counter,
        &mut used_names,
        &ancestor_names,
        &ancestor_decls,
        fbt_operands,
    );

    outlined
}

fn outline_functions_inner(
    func: &mut HIRFunction,
    outlined: &mut Vec<OutlinedFunction>,
    name_counter: &mut u32,
    used_names: &mut HashSet<String>,
    ancestor_names: &HashSet<String>,
    ancestor_decls: &HashSet<DeclarationId>,
    fbt_operands: &HashSet<IdentifierId>,
) {
    let debug_outline = std::env::var("DEBUG_OUTLINE").is_ok();

    // Build the set of names visible from this function's scope + all ancestors.
    // When checking if a child FunctionExpression captures context, we check
    // its LoadGlobal references against this set.
    let mut outer_names = ancestor_names.clone();
    collect_local_var_names(func, &mut outer_names);
    let mut outer_decls = ancestor_decls.clone();
    collect_local_decl_ids(func, &mut outer_decls);

    // Pre-scan: collect FunctionExpression IDs used as object method values.
    // Our HIR builder lowers object methods as FunctionExpression + ObjectProperty
    // with type Method, but upstream uses a distinct ObjectMethod instruction kind.
    // We must skip outlining these to match upstream behavior.
    let method_func_ids = collect_method_func_ids(func);

    for (_bid, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            // First: recurse into nested functions
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    outline_functions_inner(
                        &mut lowered_func.func,
                        outlined,
                        name_counter,
                        used_names,
                        &outer_names,
                        &outer_decls,
                        fbt_operands,
                    );
                }
                _ => {}
            }

            // Check if this FunctionExpression can be outlined
            let should_outline = match &instr.value {
                InstructionValue::FunctionExpression {
                    lowered_func,
                    name: fn_name,
                    expr_type,
                    ..
                } => {
                    let has_name = fn_name.is_some();
                    let is_decl = *expr_type == FunctionExpressionType::FunctionDeclaration;
                    let is_method = method_func_ids.contains(&instr.lvalue.identifier.id);
                    let is_fbt_operand = fbt_operands.contains(&instr.lvalue.identifier.id);
                    let captured =
                        has_captured_context(&lowered_func.func, &outer_names, &outer_decls);
                    let context_len = lowered_func.func.context.len();
                    let has_direct_context_access =
                        function_has_direct_context_access(&lowered_func.func);
                    let should =
                        !has_name && !is_decl && !is_method && !captured && !is_fbt_operand;

                    if debug_outline {
                        let context_names = lowered_func
                            .func
                            .context
                            .iter()
                            .map(|p| {
                                p.identifier.name.as_ref().map_or_else(
                                    || format!("_t{}", p.identifier.id.0),
                                    |n| n.value().to_string(),
                                )
                            })
                            .collect::<Vec<_>>();
                        eprintln!(
                            "[OUTLINE_CANDIDATE] instr={} id={} name={} decl={} method={} fbt_operand={} captured={} context_len={} direct_ctx={} context={:?} should_outline={}",
                            instr.id.0,
                            instr.lvalue.identifier.id.0,
                            has_name,
                            is_decl,
                            is_method,
                            is_fbt_operand,
                            captured,
                            context_len,
                            has_direct_context_access,
                            context_names,
                            should
                        );
                    }

                    should
                }
                _ => false,
            };

            if should_outline {
                let name = generate_temp_name(name_counter, used_names);
                used_names.insert(name.clone());

                // Extract the FunctionExpression fields
                let old_value = std::mem::replace(
                    &mut instr.value,
                    InstructionValue::Debugger {
                        loc: SourceLocation::default(),
                    },
                );

                if let InstructionValue::FunctionExpression {
                    lowered_func, loc, ..
                } = old_value
                {
                    let mut outlined_func = lowered_func.func;
                    outlined_func.id = Some(name.clone());

                    outlined.push(OutlinedFunction {
                        name: name.clone(),
                        func: outlined_func,
                    });

                    // Replace with LoadGlobal
                    instr.value = InstructionValue::LoadGlobal {
                        binding: NonLocalBinding::Global { name },
                        loc,
                    };
                }
            }
        }
    }
}

fn function_has_direct_context_access(func: &HIRFunction) -> bool {
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadContext { .. }
                | InstructionValue::StoreContext { .. }
                | InstructionValue::DeclareContext { .. } => return true,
                _ => {}
            }
        }
    }
    false
}

/// Collect IdentifierIds of FunctionExpressions used as object method values.
/// Our HIR builder lowers `{ method() { ... } }` as a FunctionExpression instruction
/// referenced by an ObjectProperty with type Method, but upstream uses a distinct
/// ObjectMethod instruction kind that is never outlined.
fn collect_method_func_ids(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut ids = HashSet::new();
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::ObjectExpression { properties, .. } = &instr.value {
                for prop in properties {
                    if let ObjectPropertyOrSpread::Property(p) = prop
                        && p.type_ == ObjectPropertyType::Method
                    {
                        ids.insert(p.place.identifier.id);
                    }
                }
            }
        }
    }
    ids
}

/// Collect all variable names declared in a function (params + local declarations).
fn collect_local_var_names(func: &HIRFunction, names: &mut HashSet<String>) {
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                    names.insert(name.clone());
                }
            }
        }
    }
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(IdentifierName::Named(name)) = &instr.lvalue.identifier.name {
                names.insert(name.clone());
            }
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. } => {
                    if let Some(IdentifierName::Named(name)) = &lvalue.place.identifier.name {
                        names.insert(name.clone());
                    }
                }
                InstructionValue::FunctionExpression { name: Some(n), .. } => {
                    names.insert(n.clone());
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_names(&lvalue.pattern, names);
                }
                _ => {}
            }
        }
    }
}

/// Collect declaration ids visible from a function body (params + locals).
fn collect_local_decl_ids(func: &HIRFunction, decls: &mut HashSet<DeclarationId>) {
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                decls.insert(p.identifier.declaration_id);
            }
        }
    }
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            decls.insert(instr.lvalue.identifier.declaration_id);
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    decls.insert(lvalue.place.identifier.declaration_id);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_decl_ids(&lvalue.pattern, decls);
                }
                _ => {}
            }
        }
    }
}

/// Check if a function captures any context from an outer scope.
///
/// Our HIR builder has two patterns for referencing outer-scope variables in
/// inner functions:
/// 1. Regular identifiers → `lower_ident_expr` → `LoadGlobal` (binding not found)
/// 2. JSX component names → `resolve_binding` → `LoadLocal` (creates fresh binding)
/// 3. Assignments to outer vars → `resolve_binding` → `StoreLocal` (fresh binding)
///
/// We detect captures by checking all three patterns against `outer_names`
/// (the set of variable names declared in any ancestor function).
///
/// We also check recursively into nested functions for transitive captures.
fn has_captured_context(
    func: &HIRFunction,
    outer_names: &HashSet<String>,
    outer_decls: &HashSet<DeclarationId>,
) -> bool {
    let debug_outline = std::env::var("DEBUG_OUTLINE").is_ok();
    // Collect names actually declared in THIS function (params + DeclareLocal).
    // StoreLocal/LoadLocal for names NOT in this set but IN outer_names = capture.
    let mut local_declared: HashSet<String> = HashSet::new();
    let mut local_declared_decls: HashSet<DeclarationId> = HashSet::new();
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                    local_declared.insert(name.clone());
                }
                local_declared_decls.insert(p.identifier.declaration_id);
            }
        }
    }
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if let Some(IdentifierName::Named(name)) = &lvalue.place.identifier.name {
                        local_declared.insert(name.clone());
                    }
                    local_declared_decls.insert(lvalue.place.identifier.declaration_id);
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    // Variable declarations are frequently lowered as StoreLocal/StoreContext
                    // with non-Reassign kind; include them as local declarations so we don't
                    // mistake declaration writes for captured outer writes.
                    if lvalue.kind != InstructionKind::Reassign {
                        if let Some(IdentifierName::Named(name)) = &lvalue.place.identifier.name {
                            local_declared.insert(name.clone());
                        }
                        local_declared_decls.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_declared_info(
                        &lvalue.pattern,
                        &mut local_declared,
                        &mut local_declared_decls,
                    );
                }
                _ => {}
            }
        }
    }

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                // LoadGlobal referencing an ancestor-scope variable = captured
                // (regular identifier references to outer vars produce LoadGlobal)
                InstructionValue::LoadGlobal { binding, .. } => {
                    if outer_names.contains(binding.name()) {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=LoadGlobal name={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                binding.name()
                            );
                        }
                        return true;
                    }
                }
                // LoadLocal referencing an outer var = captured
                // (JSX component names use resolve_binding → LoadLocal)
                InstructionValue::LoadLocal { place, .. } => {
                    if let Some(IdentifierName::Named(name)) = &place.identifier.name
                        && !local_declared.contains(name)
                        && outer_names.contains(name)
                    {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=LoadLocal name={} decl={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                name,
                                place.identifier.declaration_id.0
                            );
                        }
                        return true;
                    }
                }
                // LoadContext can reference a context value declared in this function
                // (e.g. a local captured by a nested closure), so only count names
                // that resolve to ancestors.
                InstructionValue::LoadContext { place, .. } => {
                    let decl = place.identifier.declaration_id;
                    let is_capture_by_decl =
                        outer_decls.contains(&decl) && !local_declared_decls.contains(&decl);
                    let is_capture_by_name = match &place.identifier.name {
                        Some(IdentifierName::Named(name)) => {
                            !local_declared.contains(name) && outer_names.contains(name)
                        }
                        _ => false,
                    };
                    let is_capture = is_capture_by_decl || is_capture_by_name;
                    if is_capture {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=LoadContext decl={} name={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                place.identifier.declaration_id.0,
                                place
                                    .identifier
                                    .name
                                    .as_ref()
                                    .map_or("<unnamed>", |n| n.value())
                            );
                        }
                        return true;
                    }
                }
                // StoreLocal to an outer var = captured
                // (assignments to outer vars: x = 2 where x is from parent scope)
                InstructionValue::StoreLocal { lvalue, .. } => {
                    if let Some(IdentifierName::Named(name)) = &lvalue.place.identifier.name
                        && !local_declared.contains(name)
                        && outer_names.contains(name)
                    {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=StoreLocal name={} decl={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                name,
                                lvalue.place.identifier.declaration_id.0
                            );
                        }
                        return true;
                    }
                }
                // StoreContext follows the same rule as LoadContext.
                InstructionValue::StoreContext { lvalue, .. } => {
                    let decl = lvalue.place.identifier.declaration_id;
                    let is_capture_by_decl =
                        outer_decls.contains(&decl) && !local_declared_decls.contains(&decl);
                    let is_capture_by_name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(name)) => {
                            !local_declared.contains(name) && outer_names.contains(name)
                        }
                        _ => false,
                    };
                    let is_capture = is_capture_by_decl || is_capture_by_name;
                    if is_capture {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=StoreContext decl={} name={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                lvalue.place.identifier.declaration_id.0,
                                lvalue
                                    .place
                                    .identifier
                                    .name
                                    .as_ref()
                                    .map_or("<unnamed>", |n| n.value())
                            );
                        }
                        return true;
                    }
                }
                InstructionValue::DeclareContext { lvalue, .. } => {
                    let decl = lvalue.place.identifier.declaration_id;
                    let is_capture_by_decl =
                        outer_decls.contains(&decl) && !local_declared_decls.contains(&decl);
                    let is_capture_by_name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(name)) => {
                            !local_declared.contains(name) && outer_names.contains(name)
                        }
                        _ => false,
                    };
                    if is_capture_by_decl || is_capture_by_name {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=DeclareContext decl={} name={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                lvalue.place.identifier.declaration_id.0,
                                lvalue
                                    .place
                                    .identifier
                                    .name
                                    .as_ref()
                                    .map_or("<unnamed>", |n| n.value())
                            );
                        }
                        return true;
                    }
                }
                // Recursively check nested functions for transitive captures
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    if has_captured_context(&lowered_func.func, outer_names, outer_decls) {
                        if debug_outline {
                            eprintln!(
                                "[OUTLINE_CAPTURE_REASON] fn={} kind=NestedCapture nested_fn={}",
                                func.id.as_deref().unwrap_or("<anonymous>"),
                                lowered_func.func.id.as_deref().unwrap_or("<anonymous>")
                            );
                        }
                        return true;
                    }
                }
                _ => {}
            }
        }
    }

    false
}

fn collect_pattern_declared_info(
    pattern: &Pattern,
    names: &mut HashSet<String>,
    decls: &mut HashSet<DeclarationId>,
) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                            names.insert(name.clone());
                        }
                        decls.insert(p.identifier.declaration_id);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.place.identifier.name {
                            names.insert(name.clone());
                        }
                        decls.insert(p.place.identifier.declaration_id);
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                            names.insert(name.clone());
                        }
                        decls.insert(p.identifier.declaration_id);
                    }
                }
            }
        }
    }
}

fn collect_pattern_decl_ids(pattern: &Pattern, decls: &mut HashSet<DeclarationId>) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        decls.insert(p.identifier.declaration_id);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        decls.insert(p.place.identifier.declaration_id);
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        decls.insert(p.identifier.declaration_id);
                    }
                }
            }
        }
    }
}

fn collect_pattern_names(pattern: &Pattern, names: &mut HashSet<String>) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                            names.insert(name.clone());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.place.identifier.name {
                            names.insert(name.clone());
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        if let Some(IdentifierName::Named(name)) = &p.identifier.name {
                            names.insert(name.clone());
                        }
                    }
                }
            }
        }
    }
}

fn generate_temp_name(counter: &mut u32, used: &HashSet<String>) -> String {
    loop {
        let name = if *counter == 0 {
            "_temp".to_string()
        } else {
            format!("_temp{}", counter.saturating_add(1))
        };
        *counter += 1;
        if !used.contains(&name) {
            return name;
        }
    }
}

fn collect_used_names(func: &HIRFunction, names: &mut HashSet<String>) {
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(IdentifierName::Named(name)) = &instr.lvalue.identifier.name {
                names.insert(name.clone());
            }
            match &instr.value {
                InstructionValue::FunctionExpression {
                    lowered_func, name, ..
                } => {
                    if let Some(n) = name {
                        names.insert(n.clone());
                    }
                    collect_used_names(&lowered_func.func, names);
                }
                InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_used_names(&lowered_func.func, names);
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    names.insert(binding.name().to_string());
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    #[test]
    fn generate_temp_name_matches_babel_uid_suffixes() {
        let mut counter = 0;
        let used = HashSet::new();

        assert_eq!(super::generate_temp_name(&mut counter, &used), "_temp");
        assert_eq!(super::generate_temp_name(&mut counter, &used), "_temp2");
        assert_eq!(super::generate_temp_name(&mut counter, &used), "_temp3");
    }
}
