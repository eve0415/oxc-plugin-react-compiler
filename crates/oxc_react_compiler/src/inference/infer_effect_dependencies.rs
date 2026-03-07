//! Infers reactive dependencies captured by useEffect lambdas.
//!
//! Port of `InferEffectDependencies.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::object_shape::{
    BUILT_IN_ARRAY_ID, BUILT_IN_AUTODEPS_ID, BUILT_IN_EFFECT_EVENT_ID, BUILT_IN_FIRE_FUNCTION_ID,
    BUILT_IN_SET_STATE_ID, BUILT_IN_USE_EFFECT_EVENT_ID, BUILT_IN_USE_REF_ID,
};
use crate::hir::scope_dependency_utils::build_dependency_instructions;
use crate::hir::types::*;
use crate::hir::visitors::{for_each_instruction_operand, for_each_terminal_operand};
use crate::optimization::dead_code_elimination;

/// Infers reactive dependencies captured by useEffect lambdas and adds them as
/// a second argument to the useEffect call if no dependency array is provided.
pub fn infer_effect_dependencies(func: &mut HIRFunction, retry_no_memo_mode: bool) -> bool {
    let debug_effect_deps = std::env::var("DEBUG_INFER_EFFECT_DEPS").is_ok();
    sync_env_counters(func);
    let mut fn_expressions: HashMap<IdentifierId, Instruction> = HashMap::new();

    let (autodep_fn_loads, load_globals) = collect_autodep_targets(func);
    let mut scope_infos: HashMap<ScopeId, Vec<ReactiveScopeDependency>> = HashMap::new();

    let reactive_ids = infer_reactive_identifiers(func);
    let reactive_decls = infer_reactive_declarations(func);
    let reassigned_decls = collect_reassigned_declarations(func);
    let effect_event_fn_ids = collect_effect_event_function_identifiers(func);
    let mut rewrite_blocks: Vec<BasicBlock> = Vec::new();

    // Identify useEffect-like calls and their lambdas
    for (_, block) in &func.body.blocks {
        // Track scope dependencies for simple scope terminals
        if let Terminal::Scope {
            block: inner_block,
            fallthrough,
            scope,
            ..
        } = &block.terminal
            && let Some(scope_block) = func
                .body
                .blocks
                .iter()
                .find(|(id, _)| id == inner_block)
                .map(|(_, b)| b)
            && scope_block.instructions.len() == 1
            && matches!(scope_block.terminal, Terminal::Goto { block: target, .. } if target == *fallthrough)
        {
            scope_infos.insert(scope.id, scope.dependencies.clone());
        }

        let mut rewrite_instrs: Vec<SpliceInfo> = Vec::new();

        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { .. } => {
                    fn_expressions.insert(instr.lvalue.identifier.id, instr.clone());
                }
                InstructionValue::CallExpression { callee, args, .. }
                | InstructionValue::MethodCall {
                    property: callee,
                    args,
                    ..
                } => {
                    let autodeps_expected_index = autodep_fn_loads.get(&callee.identifier.id);

                    // BuiltInAutodepsId check
                    let autodeps_arg_index = args.iter().position(|arg| {
                        if let Argument::Place(p) = arg {
                            matches!(&p.identifier.type_, Type::Object { shape_id: Some(s) } if s == BUILT_IN_AUTODEPS_ID)
                        } else {
                            false
                        }
                    });

                    if let (Some(&expected_idx), Some(actual_idx)) =
                        (autodeps_expected_index, autodeps_arg_index)
                        && actual_idx == expected_idx
                        && !args.is_empty()
                        && let Argument::Place(lambda_place) = &args[0]
                    {
                        if debug_effect_deps {
                            eprintln!(
                                "[INFER_EFFECT_DEPS] call instr#{} callee_id={} expected_idx={} actual_idx={} lambda_id={}",
                                instr.id.0,
                                callee.identifier.id.0,
                                expected_idx,
                                actual_idx,
                                lambda_place.identifier.id.0
                            );
                        }
                        let deps_place = Place {
                            identifier: func
                                .env
                                .make_temporary_identifier(SourceLocation::Generated),
                            effect: Effect::Read,
                            reactive: false,
                            loc: SourceLocation::Generated,
                        };

                        let mut effect_deps: Vec<ArrayElement> = Vec::new();

                        if let Some(fn_expr_instr) = fn_expressions.get(&lambda_place.identifier.id)
                        {
                            let mut minimal_deps = if let Some(scope) =
                                &fn_expr_instr.lvalue.identifier.scope
                            {
                                let deps = scope_infos.get(&scope.id).cloned().unwrap_or_default();
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] lambda_id={} scope_id={} direct_scope_deps={} local_scope_deps={}",
                                        lambda_place.identifier.id.0,
                                        scope.id.0,
                                        scope.dependencies.len(),
                                        deps.len()
                                    );
                                    for dep in &deps {
                                        let path = dep
                                            .path
                                            .iter()
                                            .map(|p| {
                                                if p.optional {
                                                    format!("{}?", p.property)
                                                } else {
                                                    p.property.clone()
                                                }
                                            })
                                            .collect::<Vec<_>>()
                                            .join(".");
                                        eprintln!(
                                            "[INFER_EFFECT_DEPS] scope dep ident={} decl={} path={}",
                                            dep.identifier.id.0,
                                            dep.identifier.declaration_id.0,
                                            path
                                        );
                                    }
                                }
                                if deps.is_empty() {
                                    let fallback = infer_minimal_dependencies(
                                        fn_expr_instr,
                                        debug_effect_deps,
                                    );
                                    if debug_effect_deps {
                                        eprintln!(
                                            "[INFER_EFFECT_DEPS] lambda_id={} empty scope deps; using fallback inferred deps={}",
                                            lambda_place.identifier.id.0,
                                            fallback.len()
                                        );
                                        for dep in &fallback {
                                            let path = dep
                                                .path
                                                .iter()
                                                .map(|p| {
                                                    if p.optional {
                                                        format!("{}?", p.property)
                                                    } else {
                                                        p.property.clone()
                                                    }
                                                })
                                                .collect::<Vec<_>>()
                                                .join(".");
                                            eprintln!(
                                                "[INFER_EFFECT_DEPS] fallback dep ident={} decl={} path={}",
                                                dep.identifier.id.0,
                                                dep.identifier.declaration_id.0,
                                                path
                                            );
                                        }
                                    }
                                    fallback
                                } else {
                                    let fallback = infer_minimal_dependencies(
                                        fn_expr_instr,
                                        debug_effect_deps,
                                    );
                                    if should_prefer_fallback_dependencies(&deps, &fallback) {
                                        if debug_effect_deps {
                                            eprintln!(
                                                "[INFER_EFFECT_DEPS] lambda_id={} using fallback deps for member-path precision",
                                                lambda_place.identifier.id.0
                                            );
                                            for dep in &fallback {
                                                let path = dep
                                                    .path
                                                    .iter()
                                                    .map(|p| {
                                                        if p.optional {
                                                            format!("{}?", p.property)
                                                        } else {
                                                            p.property.clone()
                                                        }
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(".");
                                                eprintln!(
                                                    "[INFER_EFFECT_DEPS] selected fallback dep ident={} decl={} path={}",
                                                    dep.identifier.id.0,
                                                    dep.identifier.declaration_id.0,
                                                    path
                                                );
                                            }
                                        }
                                        fallback
                                    } else {
                                        if debug_effect_deps {
                                            eprintln!(
                                                "[INFER_EFFECT_DEPS] lambda_id={} using scope deps",
                                                lambda_place.identifier.id.0
                                            );
                                        }
                                        deps
                                    }
                                }
                            } else {
                                // TODO: inferMinimalDependencies if scope is missing
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] lambda_id={} has no scope; using fallback inference",
                                        lambda_place.identifier.id.0
                                    );
                                }
                                infer_minimal_dependencies(fn_expr_instr, debug_effect_deps)
                            };
                            if retry_no_memo_mode {
                                minimal_deps.sort_by(|left, right| {
                                    right
                                        .identifier
                                        .id
                                        .0
                                        .cmp(&left.identifier.id.0)
                                        .then_with(|| left.path.len().cmp(&right.path.len()))
                                });
                            }

                            for maybe_dep in minimal_deps {
                                let dep_is_use_ref = is_use_ref_type(&maybe_dep.identifier);
                                let dep_is_set_state = is_set_state_type(&maybe_dep.identifier);
                                let dep_is_fire_fn = is_fire_function_type(&maybe_dep.identifier);
                                let dep_is_effect_event_fn =
                                    is_effect_event_function_type(&maybe_dep.identifier)
                                        || effect_event_fn_ids.contains(&maybe_dep.identifier.id);
                                let dep_is_reactive = reactive_ids
                                    .contains(&maybe_dep.identifier.id)
                                    || reactive_decls
                                        .contains(&maybe_dep.identifier.declaration_id);
                                let dep_is_reassigned =
                                    reassigned_decls.contains(&maybe_dep.identifier.declaration_id);
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] candidate dep ident={} decl={} type={:?} useRef={} setState={} fireFn={} effectEventFn={} reactive={} reassigned={} path={}",
                                        maybe_dep.identifier.id.0,
                                        maybe_dep.identifier.declaration_id.0,
                                        maybe_dep.identifier.type_,
                                        dep_is_use_ref,
                                        dep_is_set_state,
                                        dep_is_fire_fn,
                                        dep_is_effect_event_fn,
                                        dep_is_reactive,
                                        dep_is_reassigned,
                                        maybe_dep
                                            .path
                                            .iter()
                                            .map(|p| p.property.clone())
                                            .collect::<Vec<_>>()
                                            .join(".")
                                    );
                                }
                                if ((dep_is_set_state || (dep_is_use_ref && !retry_no_memo_mode))
                                    && !dep_is_reactive
                                    && !dep_is_reassigned)
                                    || dep_is_fire_fn
                                    || dep_is_effect_event_fn
                                {
                                    if debug_effect_deps {
                                        eprintln!(
                                            "[INFER_EFFECT_DEPS] skip dep ident={} decl={} useRef={} setState={} fireFn={} effectEventFn={} reactive={} reassigned={} path={}",
                                            maybe_dep.identifier.id.0,
                                            maybe_dep.identifier.declaration_id.0,
                                            dep_is_use_ref,
                                            dep_is_set_state,
                                            dep_is_fire_fn,
                                            dep_is_effect_event_fn,
                                            dep_is_reactive,
                                            dep_is_reassigned,
                                            maybe_dep
                                                .path
                                                .iter()
                                                .map(|p| p.property.clone())
                                                .collect::<Vec<_>>()
                                                .join(".")
                                        );
                                    }
                                    continue;
                                }
                                let dep = normalize_dependency_path(maybe_dep, retry_no_memo_mode);
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] dep ident={} path={}",
                                        dep.identifier.id.0,
                                        dep.path
                                            .iter()
                                            .map(|p| p.property.clone())
                                            .collect::<Vec<_>>()
                                            .join(".")
                                    );
                                }
                                let mut next_block_id = func.env.next_block_id();
                                let mut next_identifier_id = func.env.next_identifier_id();
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] counters before build: next_block_id={} next_identifier_id={}",
                                        next_block_id, next_identifier_id
                                    );
                                }

                                let built_deps = build_dependency_instructions(
                                    &dep,
                                    &mut next_block_id,
                                    &mut next_identifier_id,
                                );
                                if debug_effect_deps {
                                    eprintln!(
                                        "[INFER_EFFECT_DEPS] built dep place_id={} exit_block_id={} counters after build: next_block_id={} next_identifier_id={}",
                                        built_deps.place.identifier.id.0,
                                        built_deps.exit_block_id.0,
                                        next_block_id,
                                        next_identifier_id
                                    );
                                }

                                // Sync back counters
                                func.env.set_next_block_id(next_block_id);
                                func.env.set_next_identifier_id(next_identifier_id);

                                rewrite_instrs.push(SpliceInfo::Block {
                                    location: instr.id,
                                    value: built_deps.value,
                                    exit_block_id: built_deps.exit_block_id,
                                });
                                effect_deps.push(ArrayElement::Place(built_deps.place));
                            }

                            // Add the deps array instruction
                            rewrite_instrs.push(SpliceInfo::Instr {
                                location: instr.id,
                                value: Box::new(Instruction {
                                    id: InstructionId(0),
                                    lvalue: Place {
                                        identifier: deps_place.identifier.clone(),
                                        effect: Effect::Mutate,
                                        reactive: false,
                                        loc: SourceLocation::Generated,
                                    },
                                    value: InstructionValue::ArrayExpression {
                                        elements: effect_deps,
                                        loc: SourceLocation::Generated,
                                    },
                                    loc: SourceLocation::Generated,
                                    effects: None,
                                }),
                            });

                            // Replace the autodeps placeholder with the actual deps array
                            // Need to do this on the actual instruction later.
                            // For now, we'll mark this for replacement.
                            rewrite_instrs.push(SpliceInfo::ReplaceArg {
                                location: instr.id,
                                arg_index: actual_idx,
                                new_place: deps_place,
                            });
                        } else if load_globals.contains(&lambda_place.identifier.id) {
                            // Global functions have no reactive dependencies -> empty array
                            rewrite_instrs.push(SpliceInfo::Instr {
                                location: instr.id,
                                value: Box::new(Instruction {
                                    id: InstructionId(0),
                                    lvalue: Place {
                                        identifier: deps_place.identifier.clone(),
                                        effect: Effect::Mutate,
                                        reactive: false,
                                        loc: SourceLocation::Generated,
                                    },
                                    value: InstructionValue::ArrayExpression {
                                        elements: vec![],
                                        loc: SourceLocation::Generated,
                                    },
                                    loc: SourceLocation::Generated,
                                    effects: None,
                                }),
                            });
                            rewrite_instrs.push(SpliceInfo::ReplaceArg {
                                location: instr.id,
                                arg_index: actual_idx,
                                new_place: deps_place,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        rewrite_splices(block, rewrite_instrs, &mut rewrite_blocks);
    }

    if !rewrite_blocks.is_empty() {
        for block in rewrite_blocks {
            func.body.blocks.push((block.id, block));
        }
        // Restore RPO and renumber
        crate::hir::builder::reverse_postorder_blocks(&mut func.body);
        crate::hir::builder::mark_predecessors(&mut func.body);
        crate::reactive_scopes::infer_scope_variables::number_instructions(func);
        crate::hir::build_reactive_scope_terminals::fix_scope_and_identifier_ranges(&mut func.body);
        dead_code_elimination::dead_code_elimination(func);
        func.env.set_has_inferred_effect(true);
    }

    has_unresolved_effect_autodeps(func, &autodep_fn_loads)
}

fn normalize_dependency_path(
    mut dep: ReactiveScopeDependency,
    retry_no_memo_mode: bool,
) -> ReactiveScopeDependency {
    if let Some(idx) = dep.path.iter().position(|p| p.property == "current") {
        dep.path.truncate(idx);
    }
    if retry_no_memo_mode
        && dep.path.len() == 1
        && matches!(&dep.identifier.type_, Type::Object { shape_id: Some(shape) } if shape == BUILT_IN_ARRAY_ID)
    {
        dep.path.clear();
    }
    dep
}

fn is_dependency_path_prefix(prefix: &[DependencyPathEntry], path: &[DependencyPathEntry]) -> bool {
    if prefix.len() > path.len() {
        return false;
    }
    prefix
        .iter()
        .zip(path.iter())
        .all(|(lhs, rhs)| lhs.property == rhs.property && lhs.optional == rhs.optional)
}

fn is_dependency_path_equal(a: &[DependencyPathEntry], b: &[DependencyPathEntry]) -> bool {
    a.len() == b.len() && is_dependency_path_prefix(a, b)
}

fn should_prefer_fallback_dependencies(
    scope_deps: &[ReactiveScopeDependency],
    fallback_deps: &[ReactiveScopeDependency],
) -> bool {
    if scope_deps.is_empty() || fallback_deps.is_empty() {
        return false;
    }

    let scope_roots: HashSet<DeclarationId> = scope_deps
        .iter()
        .map(|dep| dep.identifier.declaration_id)
        .collect();
    let fallback_roots: HashSet<DeclarationId> = fallback_deps
        .iter()
        .map(|dep| dep.identifier.declaration_id)
        .collect();
    if scope_roots != fallback_roots {
        return false;
    }

    let mut changed = false;
    for scope_dep in scope_deps {
        let mut matched = false;
        for fallback_dep in fallback_deps {
            if fallback_dep.identifier.declaration_id != scope_dep.identifier.declaration_id {
                continue;
            }
            if is_dependency_path_equal(&fallback_dep.path, &scope_dep.path) {
                matched = true;
                break;
            }
            if is_dependency_path_prefix(&scope_dep.path, &fallback_dep.path) {
                matched = true;
                changed = true;
            }
        }
        if !matched {
            return false;
        }
    }

    changed
}

enum SpliceInfo {
    Instr {
        location: InstructionId,
        value: Box<Instruction>,
    },
    Block {
        location: InstructionId,
        value: HIR,
        exit_block_id: BlockId,
    },
    ReplaceArg {
        location: InstructionId,
        arg_index: usize,
        new_place: Place,
    },
}

fn rewrite_splices(
    original_block: &BasicBlock,
    splices: Vec<SpliceInfo>,
    rewrite_blocks: &mut Vec<BasicBlock>,
) {
    if splices.is_empty() {
        return;
    }

    let mut curr_block = original_block.clone();
    curr_block.instructions.clear();

    let mut cursor = 0;
    let original_instrs = &original_block.instructions;

    for splice in splices {
        while cursor < original_instrs.len()
            && original_instrs[cursor].id
                < match &splice {
                    SpliceInfo::Instr { location, .. } => *location,
                    SpliceInfo::Block { location, .. } => *location,
                    SpliceInfo::ReplaceArg { location, .. } => *location,
                }
        {
            curr_block
                .instructions
                .push(original_instrs[cursor].clone());
            cursor += 1;
        }

        match splice {
            SpliceInfo::Instr { value, .. } => {
                curr_block.instructions.push(*value);
            }
            SpliceInfo::Block {
                value,
                exit_block_id,
                ..
            } => {
                let entry_id = value.entry;
                let entry_block = value
                    .blocks
                    .iter()
                    .find(|(id, _)| *id == entry_id)
                    .map(|(_, b)| b)
                    .unwrap();
                curr_block
                    .instructions
                    .extend(entry_block.instructions.clone());

                if value.blocks.len() > 1 {
                    let original_terminal = curr_block.terminal.clone();
                    curr_block.terminal = entry_block.terminal.clone();
                    rewrite_blocks.push(curr_block.clone());

                    for (id, block) in value.blocks {
                        if id == entry_id {
                            continue;
                        }
                        let mut new_block = block.clone();
                        if id == exit_block_id {
                            new_block.terminal = original_terminal.clone();
                            curr_block = new_block;
                        } else {
                            rewrite_blocks.push(new_block);
                        }
                    }
                }
            }
            SpliceInfo::ReplaceArg {
                arg_index,
                new_place,
                ..
            } => {
                if cursor < original_instrs.len() {
                    let mut instr = original_instrs[cursor].clone();
                    match &mut instr.value {
                        InstructionValue::CallExpression { args, .. }
                        | InstructionValue::MethodCall { args, .. }
                        | InstructionValue::NewExpression { args, .. } => {
                            if arg_index < args.len() {
                                args[arg_index] = Argument::Place(Place {
                                    effect: Effect::Freeze,
                                    ..new_place
                                });
                            }
                        }
                        _ => {}
                    }
                    curr_block.instructions.push(instr);
                    cursor += 1;
                }
            }
        }
    }

    curr_block
        .instructions
        .extend(original_instrs[cursor..].iter().cloned());
    rewrite_blocks.push(curr_block);
}

fn infer_reactive_identifiers(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut reactive_ids = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            for_each_instruction_operand(instr, |place| {
                if place.reactive {
                    reactive_ids.insert(place.identifier.id);
                }
            });
        }
        for_each_terminal_operand(&block.terminal, |place| {
            if place.reactive {
                reactive_ids.insert(place.identifier.id);
            }
        });
    }
    reactive_ids
}

fn infer_reactive_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut reactive_decls = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if instr.lvalue.reactive {
                reactive_decls.insert(instr.lvalue.identifier.declaration_id);
            }
            for_each_instruction_operand(instr, |place| {
                if place.reactive {
                    reactive_decls.insert(place.identifier.declaration_id);
                }
            });
        }
        for_each_terminal_operand(&block.terminal, |place| {
            if place.reactive {
                reactive_decls.insert(place.identifier.declaration_id);
            }
        });
    }
    reactive_decls
}

fn collect_reassigned_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut reassigned = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        reassigned.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                _ => {}
            }
        }
    }
    reassigned
}

fn collect_effect_event_function_identifiers(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut effect_event_callee_ids = HashSet::new();
    let mut effect_event_result_ids = HashSet::new();
    let mut react_module_ids = HashSet::new();
    let mut primitive_strings: HashMap<IdentifierId, String> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } = &instr.value
            {
                primitive_strings.insert(instr.lvalue.identifier.id, s.clone());
            }
        }
    }

    loop {
        let mut changed = false;
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::LoadGlobal { binding, .. } => match binding {
                        NonLocalBinding::ImportSpecifier {
                            module, imported, ..
                        } if module == "react" && imported == "useEffectEvent" => {
                            if effect_event_callee_ids.insert(instr.lvalue.identifier.id) {
                                changed = true;
                            }
                        }
                        NonLocalBinding::ImportNamespace { module, .. }
                        | NonLocalBinding::ImportDefault { module, .. }
                            if module == "react" =>
                        {
                            if react_module_ids.insert(instr.lvalue.identifier.id) {
                                changed = true;
                            }
                        }
                        _ => {}
                    },
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        if react_module_ids.contains(&place.identifier.id)
                            && react_module_ids.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                        if effect_event_callee_ids.contains(&place.identifier.id)
                            && effect_event_callee_ids.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        if react_module_ids.contains(&value.identifier.id)
                            && react_module_ids.insert(lvalue.place.identifier.id)
                        {
                            changed = true;
                        }
                        if effect_event_callee_ids.contains(&value.identifier.id)
                            && effect_event_callee_ids.insert(lvalue.place.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::PropertyLoad {
                        object, property, ..
                    } => {
                        if react_module_ids.contains(&object.identifier.id)
                            && let PropertyLiteral::String(prop_name) = property
                            && prop_name == "useEffectEvent"
                            && effect_event_callee_ids.insert(instr.lvalue.identifier.id)
                        {
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

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::CallExpression { callee, .. } => {
                    if effect_event_callee_ids.contains(&callee.identifier.id)
                        || is_use_effect_event_callee(callee)
                    {
                        effect_event_result_ids.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::MethodCall {
                    receiver, property, ..
                } => {
                    let is_react_use_effect_event_method = react_module_ids
                        .contains(&receiver.identifier.id)
                        && primitive_strings
                            .get(&property.identifier.id)
                            .is_some_and(|prop_name| prop_name == "useEffectEvent");
                    if effect_event_callee_ids.contains(&property.identifier.id)
                        || is_use_effect_event_callee(property)
                        || is_react_use_effect_event_method
                    {
                        effect_event_result_ids.insert(instr.lvalue.identifier.id);
                    }
                }
                _ => {}
            }
        }
    }

    loop {
        let mut changed = false;
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        if effect_event_result_ids.contains(&place.identifier.id)
                            && effect_event_result_ids.insert(instr.lvalue.identifier.id)
                        {
                            changed = true;
                        }
                    }
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        if effect_event_result_ids.contains(&value.identifier.id)
                            && effect_event_result_ids.insert(lvalue.place.identifier.id)
                        {
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

    effect_event_result_ids
}

fn is_use_effect_event_callee(callee: &Place) -> bool {
    if matches!(&callee.identifier.type_, Type::Object { shape_id: Some(shape) } if shape == BUILT_IN_USE_EFFECT_EVENT_ID)
    {
        return true;
    }
    matches!(
        &callee.identifier.name,
        Some(name) if name.value() == "useEffectEvent"
    )
}

fn collect_autodep_targets(
    func: &HIRFunction,
) -> (HashMap<IdentifierId, usize>, HashSet<IdentifierId>) {
    let mut autodep_fn_configs: HashMap<String, HashMap<String, usize>> = HashMap::new();
    if let Some(configs) = &func.env.config().infer_effect_dependencies {
        for config in configs {
            autodep_fn_configs
                .entry(config.function_module.clone())
                .or_default()
                .insert(config.function_name.clone(), config.autodeps_index);
        }
    }

    let mut autodep_fn_loads: HashMap<IdentifierId, usize> = HashMap::new();
    let mut autodep_module_loads: HashMap<IdentifierId, HashMap<String, usize>> = HashMap::new();
    let mut load_globals: HashSet<IdentifierId> = HashSet::new();
    let mut primitive_strings: HashMap<IdentifierId, String> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(s),
                    ..
                } => {
                    primitive_strings.insert(instr.lvalue.identifier.id, s.clone());
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if let PropertyLiteral::String(prop_name) = property
                        && let Some(module_targets) =
                            autodep_module_loads.get(&object.identifier.id)
                        && let Some(&autodeps_index) = module_targets.get(prop_name)
                    {
                        autodep_fn_loads.insert(instr.lvalue.identifier.id, autodeps_index);
                    }
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    load_globals.insert(instr.lvalue.identifier.id);
                    match binding {
                        NonLocalBinding::ImportNamespace { module, .. } => {
                            if let Some(module_targets) = autodep_fn_configs.get(module) {
                                autodep_module_loads
                                    .insert(instr.lvalue.identifier.id, module_targets.clone());
                            }
                        }
                        NonLocalBinding::ImportSpecifier {
                            module, imported, ..
                        } => {
                            if let Some(module_targets) = autodep_fn_configs.get(module)
                                && let Some(&autodeps_index) = module_targets.get(imported)
                            {
                                autodep_fn_loads.insert(instr.lvalue.identifier.id, autodeps_index);
                            }
                        }
                        NonLocalBinding::ImportDefault { module, .. } => {
                            if let Some(module_targets) = autodep_fn_configs.get(module) {
                                // Default imports from a module object (e.g. `import React from 'react'`)
                                // should support property loads such as `React.useEffect`.
                                autodep_module_loads
                                    .insert(instr.lvalue.identifier.id, module_targets.clone());

                                if let Some(&autodeps_index) = module_targets.get("default") {
                                    autodep_fn_loads
                                        .insert(instr.lvalue.identifier.id, autodeps_index);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                InstructionValue::MethodCall {
                    receiver, property, ..
                } => {
                    if let Some(module_targets) = autodep_module_loads.get(&receiver.identifier.id)
                        && let Some(prop_name) = primitive_strings.get(&property.identifier.id)
                        && let Some(&autodeps_index) = module_targets.get(prop_name)
                    {
                        autodep_fn_loads.insert(property.identifier.id, autodeps_index);
                    }
                }
                _ => {}
            }
        }
    }

    (autodep_fn_loads, load_globals)
}

fn is_autodeps_argument(arg: &Argument) -> bool {
    match arg {
        Argument::Place(place) | Argument::Spread(place) => {
            matches!(
                &place.identifier.type_,
                Type::Object { shape_id: Some(s) } if s == BUILT_IN_AUTODEPS_ID
            )
        }
    }
}

fn has_unresolved_effect_autodeps(
    func: &HIRFunction,
    autodep_fn_loads: &HashMap<IdentifierId, usize>,
) -> bool {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::CallExpression { callee, args, .. }
                | InstructionValue::MethodCall {
                    property: callee,
                    args,
                    ..
                } => {
                    if autodep_fn_loads.contains_key(&callee.identifier.id)
                        && args.iter().any(is_autodeps_argument)
                    {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

pub fn has_mutation_after_effect_dependency_use(func: &HIRFunction) -> bool {
    let mut defs: HashMap<IdentifierId, &Instruction> = HashMap::new();
    let mut all_instrs: Vec<&Instruction> = Vec::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            defs.insert(instr.lvalue.identifier.id, instr);
            all_instrs.push(instr);
        }
    }

    all_instrs.sort_by_key(|instr| instr.id.0);

    for (idx, instr) in all_instrs.iter().enumerate() {
        let InstructionValue::CallExpression { callee, args, .. } = &instr.value else {
            continue;
        };
        if !is_effect_hook_callee(callee) || args.len() < 2 {
            continue;
        }
        let Argument::Place(dep_array_place) = &args[1] else {
            continue;
        };

        let dep_roots = resolve_effect_dependency_roots(dep_array_place, &defs);
        if dep_roots.is_empty() {
            continue;
        }

        for later in all_instrs.iter().skip(idx + 1) {
            if instruction_mutates_root_dependency(later, &dep_roots, &defs) {
                return true;
            }
        }
    }

    false
}

fn resolve_effect_dependency_roots(
    dep_array_place: &Place,
    defs: &HashMap<IdentifierId, &Instruction>,
) -> HashSet<DeclarationId> {
    let mut roots = HashSet::new();
    let Some(array_instr) = defs.get(&dep_array_place.identifier.id) else {
        return roots;
    };
    let InstructionValue::ArrayExpression { elements, .. } = &array_instr.value else {
        return roots;
    };

    for element in elements {
        match element {
            ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                let mut visited = HashSet::new();
                roots.insert(resolve_place_root_decl(place, defs, &mut visited));
            }
            ArrayElement::Hole => {}
        }
    }

    roots
}

fn resolve_place_root_decl(
    place: &Place,
    defs: &HashMap<IdentifierId, &Instruction>,
    visited: &mut HashSet<IdentifierId>,
) -> DeclarationId {
    if !visited.insert(place.identifier.id) {
        return place.identifier.declaration_id;
    }
    let Some(def_instr) = defs.get(&place.identifier.id) else {
        return place.identifier.declaration_id;
    };
    match &def_instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            resolve_place_root_decl(place, defs, visited)
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            resolve_place_root_decl(value, defs, visited)
        }
        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::ComputedLoad { object, .. } => {
            resolve_place_root_decl(object, defs, visited)
        }
        _ => def_instr.lvalue.identifier.declaration_id,
    }
}

fn instruction_mutates_root_dependency(
    instr: &Instruction,
    dep_roots: &HashSet<DeclarationId>,
    defs: &HashMap<IdentifierId, &Instruction>,
) -> bool {
    let matches_root = |place: &Place| {
        let mut visited = HashSet::new();
        dep_roots.contains(&resolve_place_root_decl(place, defs, &mut visited))
    };

    match &instr.value {
        InstructionValue::MethodCall { receiver, .. } => {
            receiver.effect != Effect::Read && matches_root(receiver)
        }
        InstructionValue::PropertyStore { object, .. }
        | InstructionValue::ComputedStore { object, .. }
        | InstructionValue::PropertyDelete { object, .. }
        | InstructionValue::ComputedDelete { object, .. } => matches_root(object),
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. } => {
            lvalue.kind == InstructionKind::Reassign && matches_root(&lvalue.place)
        }
        InstructionValue::Destructure { lvalue, .. } => {
            if lvalue.kind != InstructionKind::Reassign {
                return false;
            }
            match &lvalue.pattern {
                Pattern::Array(pattern) => pattern.items.iter().any(|elem| match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => matches_root(place),
                    ArrayElement::Hole => false,
                }),
                Pattern::Object(pattern) => pattern.properties.iter().any(|prop| match prop {
                    ObjectPropertyOrSpread::Property(prop) => matches_root(&prop.place),
                    ObjectPropertyOrSpread::Spread(place) => matches_root(place),
                }),
            }
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => matches_root(lvalue),
        _ => false,
    }
}

fn is_effect_hook_callee(callee: &Place) -> bool {
    matches!(
        &callee.identifier.type_,
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } if shape_id == "BuiltInUseEffectHookId"
            || shape_id == "BuiltInUseLayoutEffectHookId"
            || shape_id == "BuiltInUseInsertionEffectHookId"
    )
}

fn sync_env_counters(func: &HIRFunction) {
    let mut next_identifier_id = 0u32;
    let mut next_block_id = 0u32;
    max_ids_recursive(func, &mut next_identifier_id, &mut next_block_id);
    func.env.set_next_identifier_id(next_identifier_id);
    func.env.set_next_block_id(next_block_id);
}

fn max_ids_recursive(func: &HIRFunction, next_identifier_id: &mut u32, next_block_id: &mut u32) {
    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                *next_identifier_id = (*next_identifier_id).max(place.identifier.id.0 + 1);
            }
        }
    }
    for place in &func.context {
        *next_identifier_id = (*next_identifier_id).max(place.identifier.id.0 + 1);
    }
    *next_identifier_id = (*next_identifier_id).max(func.returns.identifier.id.0 + 1);

    for (block_id, block) in &func.body.blocks {
        *next_block_id = (*next_block_id).max(block_id.0 + 1);
        for phi in &block.phis {
            *next_identifier_id = (*next_identifier_id).max(phi.place.identifier.id.0 + 1);
            for operand in phi.operands.values() {
                *next_identifier_id = (*next_identifier_id).max(operand.identifier.id.0 + 1);
            }
        }
        for instr in &block.instructions {
            *next_identifier_id = (*next_identifier_id).max(instr.lvalue.identifier.id.0 + 1);
            for_each_instruction_operand(instr, |place| {
                *next_identifier_id = (*next_identifier_id).max(place.identifier.id.0 + 1);
            });
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    max_ids_recursive(&lowered_func.func, next_identifier_id, next_block_id);
                }
                _ => {}
            }
        }
        for_each_terminal_operand(&block.terminal, |place| {
            *next_identifier_id = (*next_identifier_id).max(place.identifier.id.0 + 1);
        });
    }
}

fn infer_minimal_dependencies(
    fn_expr_instr: &Instruction,
    debug_effect_deps: bool,
) -> Vec<ReactiveScopeDependency> {
    let minimal =
        crate::hir::propagate_scope_dependencies_hir::infer_minimal_dependencies_for_inner_fn(
            fn_expr_instr,
        );

    if debug_effect_deps {
        eprintln!(
            "[INFER_EFFECT_DEPS] fallback inferred {} deps for lambda_id={}",
            minimal.len(),
            fn_expr_instr.lvalue.identifier.id.0
        );
        for dep in &minimal {
            let path = dep
                .path
                .iter()
                .map(|p| p.property.clone())
                .collect::<Vec<_>>()
                .join(".");
            eprintln!(
                "[INFER_EFFECT_DEPS] fallback dep ident={} path={}",
                dep.identifier.id.0, path
            );
        }
    }

    minimal
}

fn is_use_ref_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == BUILT_IN_USE_REF_ID)
}

fn is_set_state_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Function { shape_id: Some(s), .. } if s == BUILT_IN_SET_STATE_ID)
}

fn is_fire_function_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Function { shape_id: Some(s), .. } if s == BUILT_IN_FIRE_FUNCTION_ID)
}

fn is_effect_event_function_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Function { shape_id: Some(s), .. } if s == BUILT_IN_EFFECT_EVENT_ID)
}
