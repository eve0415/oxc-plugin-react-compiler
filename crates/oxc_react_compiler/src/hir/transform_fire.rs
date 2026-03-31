//! TransformFire pass — rewrites `fire()` calls inside useEffect lambdas.
//!
//! Port of `Transform/TransformFire.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! When `enableFire` is set, this pass:
//! 1. Finds `fire(callExpr())` calls inside useEffect lambdas
//! 2. Replaces them with calls via a new `useFire` binding
//! 3. Inserts `useFire` hook calls before the useEffect call
//! 4. Updates dependency arrays and function contexts

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::environment::Environment;
use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;
use crate::hir::visitors::{for_each_instruction_operand, for_each_terminal_operand};

const USE_FIRE_FUNCTION_NAME: &str = "useFire";
const CANNOT_COMPILE_FIRE: &str = "Cannot compile `fire`";
const INVALID_FIRE_ARG_DESCRIPTION: &str = "`fire()` can only receive a function call such as `fire(fn(a,b)). Method calls and other expressions are not allowed.";
const INVALID_FIRE_DEP_ARRAY_DESCRIPTION: &str =
    "You must use an array literal for an effect dependency array when that effect uses `fire()`.";

fn debug_transform_fire_enabled() -> bool {
    std::env::var("DEBUG_TRANSFORM_FIRE").is_ok_and(|value| value == "1")
}

/// Entry point: transform fire calls in the given HIR function.
pub fn transform_fire(func: &mut HIRFunction) -> Result<(), CompilerError> {
    sync_next_identifier_id(func);
    let mut context = FireContext::new(func.env.clone());
    replace_fire_functions(func, &mut context);
    if !context.has_errors {
        ensure_no_more_fire_uses(func, &mut context);
    }
    if context.has_errors {
        let details = if context.errors.is_empty() {
            INVALID_FIRE_ARG_DESCRIPTION.to_string()
        } else {
            context.errors.join("\n")
        };
        return Err(CompilerError::Bail(BailOut {
            reason: CANNOT_COMPILE_FIRE.to_string(),
            diagnostics: vec![CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: details,
                category: ErrorCategory::Fire,
                span: None,
                ..Default::default()
            }],
        }));
    }
    Ok(())
}

fn sync_next_identifier_id(func: &HIRFunction) {
    let mut next_id = 0u32;
    max_identifier_id_recursive(func, &mut next_id);
    func.env.set_next_identifier_id(next_id);
}

fn max_identifier_id_recursive(func: &HIRFunction, next_id: &mut u32) {
    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                *next_id = (*next_id).max(place.identifier.id.0 + 1);
            }
        }
    }
    for place in &func.context {
        *next_id = (*next_id).max(place.identifier.id.0 + 1);
    }
    *next_id = (*next_id).max(func.returns.identifier.id.0 + 1);

    for (_block_id, block) in &func.body.blocks {
        for phi in &block.phis {
            *next_id = (*next_id).max(phi.place.identifier.id.0 + 1);
            for operand in phi.operands.values() {
                *next_id = (*next_id).max(operand.identifier.id.0 + 1);
            }
        }
        for instr in &block.instructions {
            *next_id = (*next_id).max(instr.lvalue.identifier.id.0 + 1);
            for_each_instruction_operand(instr, |place| {
                *next_id = (*next_id).max(place.identifier.id.0 + 1);
            });
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    max_identifier_id_recursive(&lowered_func.func, next_id);
                }
                _ => {}
            }
        }
        for_each_terminal_operand(&block.terminal, |place| {
            *next_id = (*next_id).max(place.identifier.id.0 + 1);
        });
    }
}

/// Type alias for the map of fire callees to their fire function bindings.
type FireCalleesToFireFunctionBinding = IndexMap<IdentifierId, FireCalleeInfo>;

#[derive(Clone)]
struct FireCalleeInfo {
    fire_function_binding: Place,
    captured_callee_identifier: Identifier,
}

#[derive(Clone)]
struct CallExpressionInfo {
    callee_id: IdentifierId,
    args: Vec<Argument>,
    optional: bool,
    loc: SourceLocation,
}

/// Main traversal: find and replace fire calls in a function's blocks.
fn replace_fire_functions(func: &mut HIRFunction, context: &mut FireContext) {
    let mut has_rewrite = false;
    context.push_named_binding_scope(collect_named_bindings(func));

    for (_block_id, block) in &mut func.body.blocks {
        if debug_transform_fire_enabled() {
            debug_dump_block(block);
        }
        let mut rewrite_instrs: HashMap<InstructionId, Vec<Instruction>> = HashMap::new();
        let mut delete_instrs: HashSet<InstructionId> = HashSet::new();
        // Deferred instruction value replacements: lvalue_id -> new InstructionValue
        let mut instr_value_updates: HashMap<IdentifierId, InstructionValue> = HashMap::new();
        // Deferred FunctionExpression modifications: lvalue_id -> lowered function
        let mut fn_expr_updates: HashMap<IdentifierId, LoweredFunction> = HashMap::new();
        // Deferred ArrayExpression modifications: lvalue_id -> new elements
        let mut array_expr_updates: HashMap<IdentifierId, Vec<ArrayElement>> = HashMap::new();
        // Per-block call tracking used to detect mixed fire/non-fire callee usage.
        let mut fire_wrapped_call_expr_ids: HashSet<IdentifierId> = HashSet::new();
        let mut fire_callee_source_ids: HashSet<IdentifierId> = HashSet::new();
        let mut call_expr_source_by_lvalue: HashMap<IdentifierId, IdentifierId> = HashMap::new();

        // First pass: collect info and determine rewrites
        for instr in block.instructions.iter() {
            let lvalue_id = instr.lvalue.identifier.id;

            match &instr.value {
                // Track LoadLocal instructions
                InstructionValue::LoadLocal { place, .. } => {
                    context.add_load_instruction_id(lvalue_id, instr.id);
                    context.add_load_local_instr(lvalue_id, place.clone());
                }

                // Track LoadContext instructions as callee sources as well.
                InstructionValue::LoadContext { place, .. } => {
                    context.add_load_instruction_id(lvalue_id, instr.id);
                    context.add_load_local_instr(lvalue_id, place.clone());
                }

                // Track LoadGlobal instruction ids and fire import identifiers
                InstructionValue::LoadGlobal { binding, .. } => {
                    context.add_load_instruction_id(lvalue_id, instr.id);
                    context.add_load_global_instr_id(lvalue_id, instr.id);
                    if is_fire_import_binding(binding) {
                        context.fire_identifier_ids.insert(lvalue_id);
                        if context.in_use_effect_lambda {
                            delete_instrs.insert(instr.id);
                        }
                    } else if let NonLocalBinding::Global { name } = binding {
                        context.add_global_load_id(lvalue_id);
                        let place = if let Some(identifier) =
                            context.resolve_named_binding(name).cloned()
                        {
                            Place {
                                identifier,
                                effect: Effect::Unknown,
                                reactive: false,
                                loc: instr.lvalue.loc.clone(),
                            }
                        } else {
                            context.get_or_create_global_binding_place(&instr.lvalue, name)
                        };
                        context.add_load_local_instr(lvalue_id, place);
                    }
                }

                // Track FunctionExpressions (outside useEffect lambda)
                InstructionValue::FunctionExpression { lowered_func, .. }
                    if !context.in_use_effect_lambda =>
                {
                    context.add_function_expression(lvalue_id, lowered_func.clone());
                }

                // Process FunctionExpression inside useEffect lambda (recurse)
                InstructionValue::FunctionExpression { lowered_func, .. }
                    if context.in_use_effect_lambda =>
                {
                    let mut inner_func = lowered_func.clone();
                    let _inner_callees = visit_function_expression_and_propagate_fire_dependencies(
                        &mut inner_func,
                        context,
                        false,
                    );
                    fn_expr_updates.insert(lvalue_id, inner_func);
                }

                // Track call expressions
                InstructionValue::CallExpression {
                    callee,
                    args,
                    optional,
                    loc,
                } => {
                    context.add_call_expression(
                        lvalue_id,
                        callee.identifier.id,
                        args.clone(),
                        *optional,
                        loc.clone(),
                    );
                }

                // Track ArrayExpressions
                InstructionValue::ArrayExpression { elements, .. } => {
                    context.add_array_expression(lvalue_id, elements.clone());
                }

                _ => {}
            }
        }

        // Second pass: process fire calls and useEffect calls
        for instr in block.instructions.iter() {
            match &instr.value {
                // fire(callExpr()) inside useEffect lambda
                InstructionValue::CallExpression {
                    callee, args, loc, ..
                } if context.in_use_effect_lambda
                    && context.fire_identifier_ids.contains(&callee.identifier.id) =>
                {
                    if args.len() == 1
                        && let Argument::Place(arg_place) = &args[0]
                    {
                        let call_expr = context.get_call_expression(arg_place.identifier.id);
                        if let Some(call_expr_info) = call_expr {
                            let callee_id = call_expr_info.callee_id;
                            let load_local = context.get_load_local_instr(callee_id);
                            if let Some(load_local_place) = load_local {
                                if debug_transform_fire_enabled() {
                                    eprintln!(
                                        "[TRANSFORM_FIRE] fire(call) arg_call={} callee_temp={} source={} source_decl={} source_name={}",
                                        arg_place.identifier.id.0,
                                        callee_id.0,
                                        load_local_place.identifier.id.0,
                                        load_local_place.identifier.declaration_id.0,
                                        format_identifier_name(&load_local_place.identifier),
                                    );
                                }
                                fire_wrapped_call_expr_ids.insert(arg_place.identifier.id);
                                fire_callee_source_ids.insert(load_local_place.identifier.id);
                                let fire_function_binding = context
                                    .get_or_generate_fire_function_binding(&load_local_place);

                                // If the callee came from a direct `LoadGlobal(name)`, rewriting
                                // the load itself mutates the source binding in our HIR shape.
                                // Rewrite the call-expression callee instead.
                                if context.is_global_load_id(callee_id) {
                                    instr_value_updates.insert(
                                        arg_place.identifier.id,
                                        InstructionValue::CallExpression {
                                            callee: fire_function_binding,
                                            args: call_expr_info.args,
                                            optional: call_expr_info.optional,
                                            loc: call_expr_info.loc,
                                        },
                                    );
                                    if let Some(load_instr_id) =
                                        context.get_load_instruction_id(callee_id)
                                    {
                                        delete_instrs.insert(load_instr_id);
                                    }
                                } else {
                                    instr_value_updates.insert(
                                        callee_id,
                                        InstructionValue::LoadLocal {
                                            place: fire_function_binding,
                                            loc: SourceLocation::Generated,
                                        },
                                    );
                                }

                                // Delete the fire call expression
                                delete_instrs.insert(instr.id);
                            } else {
                                context.push_error(
                                    "[InsertFire] No loadLocal found for fire call argument",
                                );
                            }
                        } else {
                            context.push_error(INVALID_FIRE_ARG_DESCRIPTION);
                        }
                    } else {
                        context.push_error(INVALID_FIRE_ARG_DESCRIPTION);
                    }
                }

                // useEffect(lambda) call
                InstructionValue::CallExpression { callee, args, .. }
                    if is_use_effect_hook_type(&callee.identifier)
                        && !args.is_empty()
                        && matches!(&args[0], Argument::Place(_)) =>
                {
                    let lambda_id = match &args[0] {
                        Argument::Place(p) => p.identifier.id,
                        _ => unreachable!(),
                    };

                    let lambda = context.get_function_expression(lambda_id);
                    if let Some(mut lambda) = lambda {
                        let captured_callees =
                            visit_function_expression_and_propagate_fire_dependencies(
                                &mut lambda,
                                context,
                                true,
                            );
                        // Add useFire calls for all fire calls found in the lambda
                        for (fire_callee_id, fire_callee_info) in &captured_callees {
                            if !context.has_callee_with_inserted_fire(*fire_callee_id) {
                                if debug_transform_fire_enabled() {
                                    eprintln!(
                                        "[TRANSFORM_FIRE] insert-useFire callee_source={} captured_id={} captured_decl={} captured_name={}",
                                        fire_callee_id.0,
                                        fire_callee_info.captured_callee_identifier.id.0,
                                        fire_callee_info
                                            .captured_callee_identifier
                                            .declaration_id
                                            .0,
                                        format_identifier_name(
                                            &fire_callee_info.captured_callee_identifier
                                        ),
                                    );
                                }
                                context.add_callee_with_inserted_fire(*fire_callee_id);

                                let mut new_instrs = Vec::new();
                                let load_use_fire = make_load_use_fire_instruction(&func.env);
                                let load_callee = make_load_fire_callee_instruction(
                                    &func.env,
                                    &fire_callee_info.captured_callee_identifier,
                                );
                                let call_use_fire = make_call_use_fire_instruction(
                                    &func.env,
                                    &load_use_fire.lvalue,
                                    &load_callee.lvalue,
                                );
                                let store_use_fire = make_store_use_fire_instruction(
                                    &func.env,
                                    &call_use_fire.lvalue,
                                    &fire_callee_info.fire_function_binding,
                                );
                                new_instrs.push(load_use_fire);
                                new_instrs.push(load_callee);
                                new_instrs.push(call_use_fire);
                                new_instrs.push(store_use_fire);

                                // Insert before the useEffect LoadGlobal
                                let load_id =
                                    context.get_load_global_instr_id(callee.identifier.id);
                                if let Some(id) = load_id {
                                    rewrite_instrs.entry(id).or_default().extend(new_instrs);
                                }
                            }
                        }

                        // Handle dep array rewriting
                        if args.len() > 1
                            && let Argument::Place(dep_array) = &args[1]
                        {
                            let dep_arr = context.get_array_expression(dep_array.identifier.id);
                            if let Some(mut elements) = dep_arr {
                                for element in &mut elements {
                                    if let ArrayElement::Place(dep_place) = element {
                                        let load_of_dep =
                                            context.get_load_local_instr(dep_place.identifier.id);
                                        if let Some(load_place) = load_of_dep
                                            && let Some(replaced_info) =
                                                captured_callees.get(&load_place.identifier.id)
                                        {
                                            *dep_place =
                                                replaced_info.fire_function_binding.clone();
                                        }
                                    }
                                }
                                array_expr_updates.insert(dep_array.identifier.id, elements);
                            } else {
                                context.push_error(INVALID_FIRE_DEP_ARRAY_DESCRIPTION);
                            }
                        } else if args.len() > 1 && matches!(&args[1], Argument::Spread(_)) {
                            context.push_error(INVALID_FIRE_DEP_ARRAY_DESCRIPTION);
                        }

                        // Write back the modified lambda
                        fn_expr_updates.insert(lambda_id, lambda);
                    }
                }

                // Track regular call expressions and their resolved source callee ids.
                InstructionValue::CallExpression {
                    callee,
                    args,
                    optional,
                    loc,
                } if !is_use_effect_hook_type(&callee.identifier)
                    && !context.fire_identifier_ids.contains(&callee.identifier.id) =>
                {
                    let source_callee_id = context
                        .get_load_local_instr(callee.identifier.id)
                        .map(|place| place.identifier.id)
                        .unwrap_or(callee.identifier.id);
                    call_expr_source_by_lvalue.insert(instr.lvalue.identifier.id, source_callee_id);
                    context.add_call_expression(
                        instr.lvalue.identifier.id,
                        callee.identifier.id,
                        args.clone(),
                        *optional,
                        loc.clone(),
                    );
                }

                _ => {}
            }
        }

        if context.in_use_effect_lambda && !fire_callee_source_ids.is_empty() {
            let mut direct_call_source_ids = HashSet::new();
            for (call_lvalue_id, source_callee_id) in call_expr_source_by_lvalue {
                if !fire_wrapped_call_expr_ids.contains(&call_lvalue_id) {
                    direct_call_source_ids.insert(source_callee_id);
                }
            }

            if let Some(conflicting_callee_id) = direct_call_source_ids
                .intersection(&fire_callee_source_ids)
                .next()
                .copied()
            {
                let callee_name = context
                    .get_load_local_instr(conflicting_callee_id)
                    .map(|place| format_identifier_name(&place.identifier))
                    .unwrap_or_else(|| "<unknown>".to_string());
                context.push_error(format!(
                    "All uses of {} must be either used with a fire() call in this effect or not used with a fire() call at all. {} was used with fire() in this effect.",
                    callee_name, callee_name
                ));
            }
        }

        // Apply rewrite instructions (insert new instructions before specified IDs)
        if !rewrite_instrs.is_empty() {
            let mut new_instructions = Vec::with_capacity(block.instructions.len());
            for instr in block.instructions.drain(..) {
                if let Some(new_instrs) = rewrite_instrs.remove(&instr.id) {
                    new_instructions.extend(new_instrs);
                }
                new_instructions.push(instr);
            }
            block.instructions = new_instructions;
            has_rewrite = true;
        }

        // Apply deletes
        if !delete_instrs.is_empty() {
            block
                .instructions
                .retain(|instr| !delete_instrs.contains(&instr.id));
            has_rewrite = true;
        }

        // Apply deferred modifications
        for instr in &mut block.instructions {
            let lvalue_id = instr.lvalue.identifier.id;

            // Apply instruction value replacements (e.g., LoadGlobal -> LoadLocal)
            if let Some(new_value) = instr_value_updates.remove(&lvalue_id) {
                instr.value = new_value;
            }

            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    if let Some(new_func) = fn_expr_updates.remove(&lvalue_id) {
                        *lowered_func = new_func;
                    }
                }
                InstructionValue::ArrayExpression { elements, .. } => {
                    if let Some(new_elements) = array_expr_updates.remove(&lvalue_id) {
                        *elements = new_elements;
                    }
                }
                _ => {}
            }
        }

        if debug_transform_fire_enabled() {
            eprintln!("[TRANSFORM_FIRE] ---- block {:?} (after) ----", block.id);
            debug_dump_block(block);
        }

        if has_rewrite {
            func.env.set_has_fire_rewrite(true);
        }
    }

    if has_rewrite {
        mark_instruction_ids(&mut func.body);
    }

    context.pop_named_binding_scope();
}

fn collect_named_bindings(func: &HIRFunction) -> HashMap<String, Identifier> {
    let mut by_name = HashMap::new();

    for arg in &func.params {
        let place = match arg {
            Argument::Place(place) | Argument::Spread(place) => place,
        };
        if let Some(name) = place.identifier.name.as_ref() {
            by_name.insert(name.value().to_string(), place.identifier.clone());
        }
    }

    for place in &func.context {
        if let Some(name) = place.identifier.name.as_ref() {
            by_name.insert(name.value().to_string(), place.identifier.clone());
        }
    }

    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if let Some(name) = lvalue.place.identifier.name.as_ref() {
                        by_name.insert(name.value().to_string(), lvalue.place.identifier.clone());
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_binding_places(&lvalue.pattern, &mut by_name);
                }
                _ => {}
            }
        }
    }
    by_name
}

fn collect_pattern_binding_places(pattern: &Pattern, by_name: &mut HashMap<String, Identifier>) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        if let Some(name) = place.identifier.name.as_ref() {
                            by_name.insert(name.value().to_string(), place.identifier.clone());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if let Some(name) = property.place.identifier.name.as_ref() {
                            by_name.insert(
                                name.value().to_string(),
                                property.place.identifier.clone(),
                            );
                        }
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if let Some(name) = place.identifier.name.as_ref() {
                            by_name.insert(name.value().to_string(), place.identifier.clone());
                        }
                    }
                }
            }
        }
    }
}

/// Traverse a function expression to find fire calls and propagate dependencies.
fn visit_function_expression_and_propagate_fire_dependencies(
    lowered_func: &mut LoweredFunction,
    context: &mut FireContext,
    entering_use_effect: bool,
) -> FireCalleesToFireFunctionBinding {
    let captured_callees = if entering_use_effect {
        context.with_use_effect_lambda_scope(|ctx| {
            replace_fire_functions(&mut lowered_func.func, ctx);
        })
    } else {
        context.with_function_scope(|ctx| {
            replace_fire_functions(&mut lowered_func.func, ctx);
        })
    };

    // Update function context to reflect new fire function bindings
    let mut replaced_context_callee_ids: HashSet<IdentifierId> = HashSet::new();
    for context_item in &mut lowered_func.func.context {
        if let Some(replaced_callee) = captured_callees.get(&context_item.identifier.id) {
            replaced_context_callee_ids.insert(context_item.identifier.id);
            *context_item = replaced_callee.fire_function_binding.clone();
        }
    }

    // Our lowering can represent captured identifiers as LoadGlobal(name) with empty
    // function contexts. Preserve fire-binding captures explicitly in that case.
    for (callee_id, callee_info) in &captured_callees {
        if replaced_context_callee_ids.contains(callee_id) {
            continue;
        }
        if lowered_func
            .func
            .context
            .iter()
            .any(|place| place.identifier.id == callee_info.fire_function_binding.identifier.id)
        {
            continue;
        }
        lowered_func
            .func
            .context
            .push(callee_info.fire_function_binding.clone());
    }

    context.merge_callees_from_inner_scope(&captured_callees);

    captured_callees
}

/// Iterate over all reachable Places in a function (including nested functions).
fn each_reachable_place(func: &HIRFunction) -> Vec<Place> {
    let mut places = Vec::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    places.extend(each_reachable_place(&lowered_func.func));
                }
                _ => {
                    for_each_instruction_operand(instr, |place| {
                        places.push(place.clone());
                    });
                }
            }
        }
    }
    places
}

fn debug_dump_block(block: &BasicBlock) {
    eprintln!("[TRANSFORM_FIRE] ---- block {:?} ----", block.id);
    for instr in &block.instructions {
        let lv_id = instr.lvalue.identifier.id.0;
        let lv_decl = instr.lvalue.identifier.declaration_id.0;
        let lv_name = format_identifier_name(&instr.lvalue.identifier);
        match &instr.value {
            InstructionValue::LoadLocal { place, .. } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=LoadLocal src={}#{}({})",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                place.identifier.id.0,
                place.identifier.declaration_id.0,
                format_identifier_name(&place.identifier),
            ),
            InstructionValue::LoadContext { place, .. } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=LoadContext src={}#{}({})",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                place.identifier.id.0,
                place.identifier.declaration_id.0,
                format_identifier_name(&place.identifier),
            ),
            InstructionValue::LoadGlobal { binding, .. } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=LoadGlobal binding={:?}",
                instr.id.0, lv_id, lv_decl, lv_name, binding
            ),
            InstructionValue::CallExpression { callee, args, .. } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=Call callee={}#{}({}) args={}",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                callee.identifier.id.0,
                callee.identifier.declaration_id.0,
                format_identifier_name(&callee.identifier),
                args.len()
            ),
            InstructionValue::StoreLocal { lvalue, value, .. } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=StoreLocal target={}#{}({}) value={}#{}({})",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                lvalue.place.identifier.id.0,
                lvalue.place.identifier.declaration_id.0,
                format_identifier_name(&lvalue.place.identifier),
                value.identifier.id.0,
                value.identifier.declaration_id.0,
                format_identifier_name(&value.identifier),
            ),
            InstructionValue::FunctionExpression {
                name, lowered_func, ..
            } => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind=FunctionExpression name={:?} context_len={}",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                name,
                lowered_func.func.context.len(),
            ),
            _ => eprintln!(
                "[TRANSFORM_FIRE] instr={} lv={}#{}({}) kind={:?}",
                instr.id.0,
                lv_id,
                lv_decl,
                lv_name,
                std::mem::discriminant(&instr.value)
            ),
        }
    }
}

fn format_identifier_name(identifier: &Identifier) -> String {
    identifier
        .name
        .as_ref()
        .map(|name| name.value().to_string())
        .unwrap_or_else(|| "<unknown>".to_string())
}

/// Ensure no remaining fire uses outside of useEffect lambdas.
fn ensure_no_more_fire_uses(func: &HIRFunction, context: &mut FireContext) {
    for place in each_reachable_place(func) {
        if is_fire_type(&place.identifier)
            || context.fire_identifier_ids.contains(&place.identifier.id)
        {
            context.push_error("Cannot use `fire` outside of a useEffect function");
        }
    }
}

/// Check if an identifier has the useEffect hook type.
fn is_use_effect_hook_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Function { shape_id: Some(shape_id), .. }
        if shape_id == "BuiltInUseEffectHookId"
    )
}

/// Check if a binding is the `fire` function from `react`.
/// In the Rust port, imports are lowered as `Global { name }`, so we match by name.
/// This is safe because the transform is gated behind `@enableFire`.
fn is_fire_import_binding(binding: &NonLocalBinding) -> bool {
    match binding {
        NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } => module == "react" && imported == "fire",
        NonLocalBinding::Global { name } => name == "fire",
        _ => false,
    }
}

/// Check if an identifier has the fire type (BuiltInFire).
fn is_fire_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Function { shape_id: Some(shape_id), .. }
        if shape_id == "BuiltInFire"
    )
}

// ---------------------------------------------------------------------------
// Instruction constructors
// ---------------------------------------------------------------------------

fn create_temporary_place(env: &Environment) -> Place {
    let identifier = env.make_temporary_identifier(SourceLocation::Generated);
    Place {
        identifier,
        effect: Effect::Unknown,
        reactive: false,
        loc: SourceLocation::Generated,
    }
}

fn promote_temporary(identifier: &mut Identifier) {
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
}

fn make_load_use_fire_instruction(env: &Environment) -> Instruction {
    let mut use_fire_place = create_temporary_place(env);
    use_fire_place.effect = Effect::Read;
    use_fire_place.identifier.type_ = Type::Function {
        shape_id: Some("BuiltInDefaultNonmutatingHookId".to_string()),
        return_type: Box::new(Type::Poly),
        is_constructor: false,
    };

    Instruction {
        id: InstructionId(0),
        lvalue: use_fire_place,
        value: InstructionValue::LoadGlobal {
            binding: NonLocalBinding::ImportSpecifier {
                name: USE_FIRE_FUNCTION_NAME.to_string(),
                module: "react/compiler-runtime".to_string(),
                imported: USE_FIRE_FUNCTION_NAME.to_string(),
            },
            loc: SourceLocation::Generated,
        },
        loc: SourceLocation::Generated,
        effects: None,
    }
}

fn make_load_fire_callee_instruction(
    env: &Environment,
    fire_callee_identifier: &Identifier,
) -> Instruction {
    let loaded_fire_callee = create_temporary_place(env);
    let fire_callee = Place {
        identifier: fire_callee_identifier.clone(),
        reactive: false,
        effect: Effect::Unknown,
        loc: fire_callee_identifier.loc.clone(),
    };

    Instruction {
        id: InstructionId(0),
        lvalue: loaded_fire_callee,
        value: InstructionValue::LoadLocal {
            place: fire_callee,
            loc: SourceLocation::Generated,
        },
        loc: SourceLocation::Generated,
        effects: None,
    }
}

fn make_call_use_fire_instruction(
    env: &Environment,
    use_fire_place: &Place,
    arg_place: &Place,
) -> Instruction {
    let mut result_place = create_temporary_place(env);
    result_place.effect = Effect::Read;

    Instruction {
        id: InstructionId(0),
        lvalue: result_place,
        value: InstructionValue::CallExpression {
            callee: use_fire_place.clone(),
            args: vec![Argument::Place(arg_place.clone())],
            optional: false,
            loc: SourceLocation::Generated,
        },
        loc: SourceLocation::Generated,
        effects: None,
    }
}

fn make_store_use_fire_instruction(
    env: &Environment,
    use_fire_call_result_place: &Place,
    fire_function_binding_place: &Place,
) -> Instruction {
    let mut binding_place = fire_function_binding_place.clone();
    promote_temporary(&mut binding_place.identifier);

    let lvalue_place = create_temporary_place(env);

    Instruction {
        id: InstructionId(0),
        lvalue: lvalue_place,
        value: InstructionValue::StoreLocal {
            lvalue: LValue {
                kind: InstructionKind::Const,
                place: binding_place,
            },
            value: use_fire_call_result_place.clone(),
            loc: SourceLocation::Generated,
        },
        loc: SourceLocation::Generated,
        effects: None,
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct FireContext {
    env: Environment,
    has_errors: bool,
    errors: Vec<String>,

    /// Identifier ids that were loaded from the `fire` import from `react`.
    fire_identifier_ids: HashSet<IdentifierId>,

    /// Tracks call expressions by their lvalue id.
    call_expressions: HashMap<IdentifierId, CallExpressionInfo>,

    /// Tracks function expressions by their lvalue id.
    function_expressions: HashMap<IdentifierId, LoweredFunction>,

    /// Tracks LoadLocal instructions by lvalue id -> the Place being loaded.
    load_locals: HashMap<IdentifierId, Place>,
    /// Tracks load instruction ids by lvalue id.
    load_instruction_ids: HashMap<IdentifierId, InstructionId>,
    /// Canonical per-name identifiers for `LoadGlobal { name }` accesses.
    global_binding_identifiers: HashMap<String, Identifier>,
    /// Lvalue ids that originated from `LoadGlobal { name }`.
    global_load_ids: HashSet<IdentifierId>,

    /// Maps fire callees to generated fire function places.
    fire_callees_to_fire_functions: IndexMap<IdentifierId, Place>,

    /// Callees for which we've already inserted useFire.
    callees_with_inserted_fire: HashSet<IdentifierId>,

    /// Captured callee identifier ids for the current scope.
    captured_callee_identifier_ids: FireCalleesToFireFunctionBinding,

    /// Whether we're inside a useEffect lambda.
    in_use_effect_lambda: bool,

    /// Maps LoadGlobal lvalue ids to instruction ids.
    load_global_instruction_ids: HashMap<IdentifierId, InstructionId>,

    /// Tracks array expressions by lvalue id.
    array_expressions: HashMap<IdentifierId, Vec<ArrayElement>>,

    /// Lexical name-resolution stack used to map lowered `LoadGlobal(name)`
    /// back to enclosing local bindings.
    named_binding_scopes: Vec<HashMap<String, Identifier>>,
}

impl FireContext {
    fn new(env: Environment) -> Self {
        Self {
            env,
            has_errors: false,
            errors: Vec::new(),
            fire_identifier_ids: HashSet::new(),
            call_expressions: HashMap::new(),
            function_expressions: HashMap::new(),
            load_locals: HashMap::new(),
            load_instruction_ids: HashMap::new(),
            global_binding_identifiers: HashMap::new(),
            global_load_ids: HashSet::new(),
            fire_callees_to_fire_functions: IndexMap::new(),
            callees_with_inserted_fire: HashSet::new(),
            captured_callee_identifier_ids: IndexMap::new(),
            in_use_effect_lambda: false,
            load_global_instruction_ids: HashMap::new(),
            array_expressions: HashMap::new(),
            named_binding_scopes: Vec::new(),
        }
    }

    fn push_error(&mut self, message: impl Into<String>) {
        self.has_errors = true;
        let message = message.into();
        if debug_transform_fire_enabled() {
            eprintln!("[TRANSFORM_FIRE] error: {}", message);
        }
        self.errors.push(message);
    }

    fn with_function_scope(
        &mut self,
        f: impl FnOnce(&mut FireContext),
    ) -> FireCalleesToFireFunctionBinding {
        f(self);
        std::mem::take(&mut self.captured_callee_identifier_ids)
    }

    fn with_use_effect_lambda_scope(
        &mut self,
        f: impl FnOnce(&mut FireContext),
    ) -> FireCalleesToFireFunctionBinding {
        let saved_captured = std::mem::take(&mut self.captured_callee_identifier_ids);
        let saved_in_use_effect = self.in_use_effect_lambda;

        self.captured_callee_identifier_ids = IndexMap::new();
        self.in_use_effect_lambda = true;

        let result = self.with_function_scope(f);

        self.captured_callee_identifier_ids = saved_captured;
        self.in_use_effect_lambda = saved_in_use_effect;

        result
    }

    fn add_call_expression(
        &mut self,
        id: IdentifierId,
        callee_id: IdentifierId,
        args: Vec<Argument>,
        optional: bool,
        loc: SourceLocation,
    ) {
        self.call_expressions.insert(
            id,
            CallExpressionInfo {
                callee_id,
                args,
                optional,
                loc,
            },
        );
    }

    fn get_call_expression(&self, id: IdentifierId) -> Option<CallExpressionInfo> {
        self.call_expressions.get(&id).cloned()
    }

    fn add_load_local_instr(&mut self, id: IdentifierId, place: Place) {
        self.load_locals.insert(id, place);
    }

    fn add_load_instruction_id(&mut self, id: IdentifierId, instr_id: InstructionId) {
        self.load_instruction_ids.insert(id, instr_id);
    }

    fn get_load_local_instr(&self, id: IdentifierId) -> Option<Place> {
        self.load_locals.get(&id).cloned()
    }

    fn get_load_instruction_id(&self, id: IdentifierId) -> Option<InstructionId> {
        self.load_instruction_ids.get(&id).copied()
    }

    fn get_or_create_global_binding_place(&mut self, lvalue: &Place, name: &str) -> Place {
        let canonical_identifier = self
            .global_binding_identifiers
            .entry(name.to_string())
            .or_insert_with(|| {
                let mut identifier = lvalue.identifier.clone();
                identifier.name = Some(IdentifierName::Named(name.to_string()));
                identifier
            })
            .clone();

        Place {
            identifier: canonical_identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc: lvalue.loc.clone(),
        }
    }

    fn add_global_load_id(&mut self, id: IdentifierId) {
        self.global_load_ids.insert(id);
    }

    fn is_global_load_id(&self, id: IdentifierId) -> bool {
        self.global_load_ids.contains(&id)
    }

    fn get_or_generate_fire_function_binding(&mut self, callee: &Place) -> Place {
        let fire_function_binding = self
            .fire_callees_to_fire_functions
            .entry(callee.identifier.id)
            .or_insert_with(|| create_temporary_place(&self.env))
            .clone();

        let mut binding = fire_function_binding;
        // Upstream mutates the shared fire binding place via promoteTemporary(),
        // so every rewritten call/load observes the promoted identifier name.
        // In Rust we must persist that mutation on the canonical binding explicitly.
        promote_temporary(&mut binding.identifier);
        binding.identifier.type_ = Type::Function {
            shape_id: Some("BuiltInFireFunction".to_string()),
            return_type: Box::new(Type::Poly),
            is_constructor: false,
        };

        self.fire_callees_to_fire_functions
            .insert(callee.identifier.id, binding.clone());

        self.captured_callee_identifier_ids.insert(
            callee.identifier.id,
            FireCalleeInfo {
                fire_function_binding: binding.clone(),
                captured_callee_identifier: callee.identifier.clone(),
            },
        );

        binding
    }

    fn merge_callees_from_inner_scope(&mut self, inner_callees: &FireCalleesToFireFunctionBinding) {
        for (id, callee_info) in inner_callees {
            self.captured_callee_identifier_ids
                .insert(*id, callee_info.clone());
        }
    }

    fn add_callee_with_inserted_fire(&mut self, id: IdentifierId) {
        self.callees_with_inserted_fire.insert(id);
    }

    fn has_callee_with_inserted_fire(&self, id: IdentifierId) -> bool {
        self.callees_with_inserted_fire.contains(&id)
    }

    fn add_function_expression(&mut self, id: IdentifierId, func: LoweredFunction) {
        self.function_expressions.insert(id, func);
    }

    fn get_function_expression(&self, id: IdentifierId) -> Option<LoweredFunction> {
        self.function_expressions.get(&id).cloned()
    }

    fn add_load_global_instr_id(&mut self, id: IdentifierId, instr_id: InstructionId) {
        self.load_global_instruction_ids.insert(id, instr_id);
    }

    fn get_load_global_instr_id(&self, id: IdentifierId) -> Option<InstructionId> {
        self.load_global_instruction_ids.get(&id).copied()
    }

    fn add_array_expression(&mut self, id: IdentifierId, elements: Vec<ArrayElement>) {
        self.array_expressions.insert(id, elements);
    }

    fn get_array_expression(&self, id: IdentifierId) -> Option<Vec<ArrayElement>> {
        self.array_expressions.get(&id).cloned()
    }

    fn push_named_binding_scope(&mut self, bindings: HashMap<String, Identifier>) {
        self.named_binding_scopes.push(bindings);
    }

    fn pop_named_binding_scope(&mut self) {
        let _ = self.named_binding_scopes.pop();
    }

    fn resolve_named_binding(&self, name: &str) -> Option<&Identifier> {
        for scope in self.named_binding_scopes.iter().rev() {
            if let Some(identifier) = scope.get(name) {
                return Some(identifier);
            }
        }
        None
    }
}
