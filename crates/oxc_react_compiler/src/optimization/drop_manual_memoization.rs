//! Drop manual memoization — removes useMemo/useCallback wrappers.
//!
//! Port of `DropManualMemoization.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Converts:
//!   useMemo(fn, deps) → fn()
//!   useCallback(fn, deps) → fn
//!
//! When validation is enabled (default: `validateNoSetStateInRender = true`),
//! this pass also inserts `StartMemoize`/`FinishMemoize` markers that preserve
//! the original dependency list. This keeps deps alive through DCE so that
//! destructured parameters referenced only in useMemo/useCallback dep arrays
//! are not incorrectly pruned.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;

/// Drop useMemo/useCallback wrappers in the HIR.
///
/// Handles both direct calls (`useMemo(...)`) and method calls (`React.useMemo(...)`).
/// Inserts StartMemoize/FinishMemoize markers to preserve dep liveness through DCE,
/// matching upstream behavior when `validateNoSetStateInRender` (default: true) is enabled.
///
/// When validation is enabled, this pass also validates that the first argument
/// to useMemo/useCallback is an inline function expression.
pub fn drop_manual_memoization(func: &mut HIRFunction) -> Result<(), CompilerError> {
    let debug_manual_memo = std::env::var("DEBUG_MANUAL_MEMO").is_ok();
    let debug_manual_memo_deps = std::env::var("DEBUG_MANUAL_MEMO_DEPS").is_ok();
    let debug_manual_memo_dump = std::env::var("DEBUG_MANUAL_MEMO_DUMP").is_ok();
    let config = func.env.config();
    let is_validation_enabled = config.validate_preserve_existing_memoization_guarantees
        || config.validate_no_set_state_in_render
        || config.enable_preserve_existing_memoization_guarantees;
    let mut errors: Vec<CompilerDiagnostic> = Vec::new();
    // Phase 1: Collect identifiers that refer to useMemo/useCallback/React,
    // and collect maybe-deps information from LoadLocal/PropertyLoad/ArrayExpression.
    let mut memo_hooks: HashSet<IdentifierId> = HashSet::new();
    let mut memo_kinds: HashMap<IdentifierId, MemoKind> = HashMap::new();
    let mut react_ids: HashSet<IdentifierId> = HashSet::new();
    // Track the instruction ID of the LoadGlobal/PropertyLoad that loaded useMemo/useCallback
    let mut memo_load_instr_ids: HashMap<IdentifierId, InstructionId> = HashMap::new();

    // Maps to track dependency information (matching upstream collectTemporaries)
    let mut maybe_deps_lists: HashMap<IdentifierId, Vec<Place>> = HashMap::new();
    let mut maybe_deps: HashMap<IdentifierId, ManualMemoDependency> = HashMap::new();
    let mut store_targets: HashMap<IdentifierId, IdentifierId> = HashMap::new();
    let mut dep_defs: HashMap<IdentifierId, String> = HashMap::new();
    // Track functions for the isValidationEnabled check
    let mut functions: HashSet<IdentifierId> = HashSet::new();
    // Track Primitive string values by ID for MethodCall property detection.
    // Our HIR builder lowers `obj.method(args)` as MethodCall { property: Primitive("method") }
    // rather than PropertyLoad + CallExpression, so we need to detect React.useMemo/useCallback
    // through the Primitive instruction.
    let mut primitive_strings: HashMap<IdentifierId, String> = HashMap::new();
    // Track primitive literal values used as computed property keys so
    // `obj["x"]` / `obj[0]` can participate in manual-memo deps extraction.
    let mut primitive_property_literals: HashMap<IdentifierId, String> = HashMap::new();
    let optional_places = find_optional_places(func);
    if debug_manual_memo_deps {
        let mut optional_ids: Vec<u32> = optional_places.iter().map(|id| id.0).collect();
        optional_ids.sort_unstable();
        eprintln!("[MANUAL_MEMO_DEPS] optional_places={:?}", optional_ids);
    }

    let mut next_manual_memo_id: u32 = 0;

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let lvalue_is_optional = optional_places.contains(&instr.lvalue.identifier.id);
            if debug_manual_memo_deps {
                dep_defs.insert(instr.lvalue.identifier.id, format!("{:?}", instr.value));
            }
            match &instr.value {
                InstructionValue::FunctionExpression { .. } => {
                    functions.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name();
                    if is_hook_alias_name(name, "useMemo") {
                        memo_hooks.insert(instr.lvalue.identifier.id);
                        memo_kinds.insert(instr.lvalue.identifier.id, MemoKind::UseMemo);
                        memo_load_instr_ids.insert(instr.lvalue.identifier.id, instr.id);
                    } else if is_hook_alias_name(name, "useCallback") {
                        memo_hooks.insert(instr.lvalue.identifier.id);
                        memo_kinds.insert(instr.lvalue.identifier.id, MemoKind::UseCallback);
                        memo_load_instr_ids.insert(instr.lvalue.identifier.id, instr.id);
                    } else if name == "React" {
                        react_ids.insert(instr.lvalue.identifier.id);
                    }
                    // Track as maybe-dep (global)
                    maybe_deps.insert(
                        instr.lvalue.identifier.id,
                        ManualMemoDependency {
                            root: ManualMemoRoot::Global {
                                identifier_name: name.to_string(),
                            },
                            path: vec![],
                        },
                    );
                }
                InstructionValue::PropertyLoad {
                    object,
                    property,
                    optional,
                    ..
                } => {
                    if react_ids.contains(&object.identifier.id)
                        && let PropertyLiteral::String(prop_name) = property
                    {
                        if prop_name == "useMemo" {
                            memo_hooks.insert(instr.lvalue.identifier.id);
                            memo_kinds.insert(instr.lvalue.identifier.id, MemoKind::UseMemo);
                            memo_load_instr_ids.insert(instr.lvalue.identifier.id, instr.id);
                        } else if prop_name == "useCallback" {
                            memo_hooks.insert(instr.lvalue.identifier.id);
                            memo_kinds.insert(instr.lvalue.identifier.id, MemoKind::UseCallback);
                            memo_load_instr_ids.insert(instr.lvalue.identifier.id, instr.id);
                        }
                    }
                    // Track as maybe-dep (property load)
                    if let Some(obj_dep) = maybe_deps.get(&object.identifier.id) {
                        let prop_str = match property {
                            PropertyLiteral::String(s) => s.clone(),
                            PropertyLiteral::Number(n) => n.to_string(),
                        };
                        let mut path = obj_dep.path.clone();
                        let mut entry_optional = lvalue_is_optional;
                        if *optional {
                            if let Some(prev) = path.last_mut() {
                                prev.optional = true;
                            } else {
                                entry_optional = true;
                            }
                        }
                        path.push(DependencyPathEntry {
                            property: prop_str,
                            optional: entry_optional,
                        });
                        maybe_deps.insert(
                            instr.lvalue.identifier.id,
                            ManualMemoDependency {
                                root: obj_dep.root.clone(),
                                path,
                            },
                        );
                    }
                }
                InstructionValue::ComputedLoad {
                    object,
                    property,
                    optional,
                    ..
                } => {
                    if let Some(obj_dep) = maybe_deps.get(&object.identifier.id)
                        && let Some(prop_str) =
                            primitive_property_literals.get(&property.identifier.id)
                    {
                        let mut path = obj_dep.path.clone();
                        let mut entry_optional = lvalue_is_optional;
                        if *optional {
                            if let Some(prev) = path.last_mut() {
                                prev.optional = true;
                            } else {
                                entry_optional = true;
                            }
                        }
                        path.push(DependencyPathEntry {
                            property: prop_str.clone(),
                            optional: entry_optional,
                        });
                        maybe_deps.insert(
                            instr.lvalue.identifier.id,
                            ManualMemoDependency {
                                root: obj_dep.root.clone(),
                                path,
                            },
                        );
                    }
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    // Track as maybe-dep (local variable)
                    if let Some(source) = maybe_deps.get(&place.identifier.id) {
                        maybe_deps.insert(instr.lvalue.identifier.id, source.clone());
                    } else if place.identifier.name.is_some() {
                        maybe_deps.insert(
                            instr.lvalue.identifier.id,
                            ManualMemoDependency {
                                root: ManualMemoRoot::NamedLocal(place.clone()),
                                path: vec![],
                            },
                        );
                    }
                }
                InstructionValue::ArrayExpression { elements, .. } => {
                    // Track as maybe-deps list if all elements are identifiers (not spreads/holes)
                    let all_identifiers =
                        elements.iter().all(|e| matches!(e, ArrayElement::Place(_)));
                    if all_identifiers {
                        let places: Vec<Place> = elements
                            .iter()
                            .filter_map(|e| match e {
                                ArrayElement::Place(p) => Some(p.clone()),
                                _ => None,
                            })
                            .collect();
                        maybe_deps_lists.insert(instr.lvalue.identifier.id, places);
                    }
                }
                InstructionValue::Primitive { value, .. } => match value {
                    PrimitiveValue::String(s) => {
                        primitive_strings.insert(instr.lvalue.identifier.id, s.clone());
                        primitive_property_literals.insert(instr.lvalue.identifier.id, s.clone());
                    }
                    PrimitiveValue::Number(n) => {
                        primitive_property_literals
                            .insert(instr.lvalue.identifier.id, n.to_string());
                    }
                    PrimitiveValue::Null
                    | PrimitiveValue::Undefined
                    | PrimitiveValue::Boolean(_) => {}
                },
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    store_targets.insert(instr.lvalue.identifier.id, lvalue.place.identifier.id);
                    // Track store aliases for value block chains (local/context).
                    if let Some(aliased) = maybe_deps.get(&value.identifier.id)
                        && lvalue.place.identifier.name.is_none()
                    {
                        maybe_deps.insert(lvalue.place.identifier.id, aliased.clone());
                    }
                }
                InstructionValue::Ternary {
                    consequent,
                    alternate,
                    ..
                } => {
                    let consequent_dep = maybe_deps.get(&consequent.identifier.id).cloned();
                    let alternate_dep = maybe_deps.get(&alternate.identifier.id).cloned();
                    let inferred = match (consequent_dep, alternate_dep) {
                        (Some(cdep), Some(adep)) => {
                            if manual_memo_dependency_eq(&cdep, &adep) {
                                Some(cdep)
                            } else {
                                None
                            }
                        }
                        (Some(cdep), None) => {
                            if dep_matches_store_target(
                                &cdep,
                                alternate.identifier.id,
                                &store_targets,
                            ) {
                                Some(cdep)
                            } else {
                                None
                            }
                        }
                        (None, Some(adep)) => {
                            if dep_matches_store_target(
                                &adep,
                                consequent.identifier.id,
                                &store_targets,
                            ) {
                                Some(adep)
                            } else {
                                None
                            }
                        }
                        (None, None) => None,
                    };
                    if let Some(dep) = inferred {
                        if debug_manual_memo_deps {
                            eprintln!(
                                "[MANUAL_MEMO_DEPS] ternary_fallback lvalue={} consequent={} alternate={}",
                                instr.lvalue.identifier.id.0,
                                consequent.identifier.id.0,
                                alternate.identifier.id.0
                            );
                        }
                        maybe_deps.insert(instr.lvalue.identifier.id, dep);
                    }
                }
                _ => {}
            }
        }
    }
    if debug_manual_memo_dump {
        eprintln!("[MANUAL_MEMO_DUMP] function={:?}", func.id);
        for (block_id, block) in &func.body.blocks {
            eprintln!(
                "[MANUAL_MEMO_DUMP] block#{} instr_count={}",
                block_id.0,
                block.instructions.len()
            );
            for instr in &block.instructions {
                eprintln!(
                    "[MANUAL_MEMO_DUMP]   instr#{} lvalue={} value={:?}",
                    instr.id.0, instr.lvalue.identifier.id.0, instr.value
                );
            }
        }
    }

    // Phase 2: Collect transforms and marker insertions.
    // We need to collect all changes first, then apply them, because we can't
    // mutate blocks while iterating.
    struct PendingChange {
        block_id: BlockId,
        instr_idx: usize,
        transform: MemoTransform,
        // Optional StartMemoize/FinishMemoize markers to insert
        start_marker: Option<Instruction>,
        finish_marker: Option<Instruction>,
        // The instruction ID after which to insert the start marker
        start_after_instr_id: Option<InstructionId>,
    }

    let mut changes: Vec<PendingChange> = Vec::new();

    for (block_id, block) in &func.body.blocks {
        for (i, instr) in block.instructions.iter().enumerate() {
            let (callee_id, args, method_call_info) = match &instr.value {
                InstructionValue::CallExpression { callee, args, .. } => {
                    (callee.identifier.id, args, None)
                }
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    args,
                    ..
                } => (
                    property.identifier.id,
                    args,
                    Some((receiver.identifier.id, property.identifier.id)),
                ),
                _ => continue,
            };

            // For MethodCall with receiver in react_ids and property being a
            // Primitive "useMemo"/"useCallback", register as memo hook on the fly.
            // This handles our HIR builder which creates MethodCall with Primitive
            // property values rather than PropertyLoad + CallExpression.
            if let Some((receiver_id, prop_id)) = method_call_info
                && react_ids.contains(&receiver_id)
                && !memo_hooks.contains(&prop_id)
                && let Some(prop_name) = primitive_strings.get(&prop_id)
            {
                if prop_name == "useMemo" {
                    memo_hooks.insert(prop_id);
                    memo_kinds.insert(prop_id, MemoKind::UseMemo);
                    memo_load_instr_ids.insert(prop_id, instr.id);
                } else if prop_name == "useCallback" {
                    memo_hooks.insert(prop_id);
                    memo_kinds.insert(prop_id, MemoKind::UseCallback);
                    memo_load_instr_ids.insert(prop_id, instr.id);
                }
            }

            if !memo_hooks.contains(&callee_id) {
                continue;
            }
            let kind = match memo_kinds.get(&callee_id) {
                Some(k) => *k,
                None => continue,
            };
            let first_arg = match args.first() {
                Some(Argument::Place(place)) => place,
                _ => continue,
            };

            let transform = match kind {
                MemoKind::UseMemo => MemoTransform::UseMemoFnRef {
                    callee: first_arg.clone(),
                },
                MemoKind::UseCallback => MemoTransform::UseCallbackFnRef {
                    callback: first_arg.clone(),
                },
            };

            // Extract deps list for StartMemoize/FinishMemoize markers
            let deps_list = extract_deps_list(
                kind,
                args,
                &maybe_deps_lists,
                &maybe_deps,
                &dep_defs,
                debug_manual_memo_deps,
                &mut errors,
            );
            let has_deps = deps_list.is_some();
            let load_instr_id = memo_load_instr_ids.get(&callee_id).copied();

            // When validation is enabled, check that the first argument is an
            // inline function expression. If not, bail out with an error.
            // This matches upstream DropManualMemoization behavior.
            let skip_markers = if is_validation_enabled {
                if !functions.contains(&first_arg.identifier.id) {
                    errors.push(CompilerDiagnostic {
                        severity: DiagnosticSeverity::InvalidReact,
                        message: "Expected the first argument to be an inline function expression"
                            .to_string(),
                    });
                    true
                } else {
                    false
                }
            } else {
                false
            };

            let (start_marker, finish_marker) = if skip_markers {
                (None, None)
            } else {
                let memo_id = next_manual_memo_id;
                next_manual_memo_id += 1;

                let fn_place = first_arg;
                let loc = instr.loc.clone();

                let start = Instruction {
                    id: InstructionId(0), // Will be renumbered
                    lvalue: make_temporary_identifier(),
                    value: InstructionValue::StartMemoize {
                        manual_memo_id: memo_id,
                        deps: deps_list.clone(),
                        loc: loc.clone(),
                    },
                    loc: loc.clone(),
                    effects: None,
                };

                let memo_decl = match kind {
                    MemoKind::UseMemo => instr.lvalue.clone(),
                    MemoKind::UseCallback => Place {
                        identifier: fn_place.identifier.clone(),
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: fn_place.loc.clone(),
                    },
                };

                let finish = Instruction {
                    id: InstructionId(0),
                    lvalue: make_temporary_identifier(),
                    value: InstructionValue::FinishMemoize {
                        manual_memo_id: memo_id,
                        decl: memo_decl,
                        pruned: false,
                        loc: loc.clone(),
                    },
                    loc,
                    effects: None,
                };

                (Some(start), Some(finish))
            };
            if debug_manual_memo {
                let kind_name = match kind {
                    MemoKind::UseMemo => "useMemo",
                    MemoKind::UseCallback => "useCallback",
                };
                eprintln!(
                    "[MANUAL_MEMO] transform instr#{} kind={} callee_id={} has_deps={} skip_markers={} start_after={:?}",
                    instr.id.0,
                    kind_name,
                    callee_id.0,
                    has_deps,
                    skip_markers,
                    load_instr_id.map(|id| id.0)
                );
            }

            changes.push(PendingChange {
                block_id: *block_id,
                instr_idx: i,
                transform,
                start_marker,
                finish_marker,
                start_after_instr_id: load_instr_id,
            });
        }
    }

    // Phase 3: Apply transforms and insert markers.
    for change in &changes {
        let block = func
            .body
            .blocks
            .iter_mut()
            .find(|(id, _)| *id == change.block_id)
            .map(|(_, b)| b)
            .expect("Block not found");

        apply_transform(&mut block.instructions, change.instr_idx, &change.transform);
    }

    // Insert markers (StartMemoize after load, FinishMemoize after call).
    // Mirror upstream behavior: keyed by instruction id, so if multiple
    // markers target the same instruction, the most-recent one wins.
    let mut queued_inserts: HashMap<InstructionId, Instruction> = HashMap::new();
    for change in &changes {
        if let Some(finish) = &change.finish_marker {
            let block = func
                .body
                .blocks
                .iter()
                .find(|(id, _)| *id == change.block_id)
                .map(|(_, b)| b)
                .expect("Block not found");
            let call_instr_id = block.instructions[change.instr_idx].id;
            queued_inserts.insert(call_instr_id, finish.clone());
        }
        if let Some(start) = &change.start_marker
            && let Some(after_id) = change.start_after_instr_id
        {
            queued_inserts.insert(after_id, start.clone());
        }
    }

    // Apply insertions by block. This inserts at most one marker after each
    // instruction id, matching upstream Map semantics.
    let mut has_changes = false;
    if !queued_inserts.is_empty() {
        for (_, block) in &mut func.body.blocks {
            let mut next_instructions: Option<Vec<Instruction>> = None;
            for (i, instr) in block.instructions.iter().enumerate() {
                if let Some(insert_instr) = queued_inserts.get(&instr.id) {
                    let next =
                        next_instructions.get_or_insert_with(|| block.instructions[..i].to_vec());
                    next.push(instr.clone());
                    next.push(insert_instr.clone());
                } else if let Some(next) = next_instructions.as_mut() {
                    next.push(instr.clone());
                }
            }
            if let Some(next) = next_instructions {
                block.instructions = next;
                has_changes = true;
            }
        }
    }
    if has_changes {
        mark_instruction_ids(&mut func.body);
    }
    if debug_manual_memo {
        let mut start_count = 0usize;
        let mut finish_count = 0usize;
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match instr.value {
                    InstructionValue::StartMemoize { .. } => start_count += 1,
                    InstructionValue::FinishMemoize { .. } => finish_count += 1,
                    _ => {}
                }
            }
        }
        eprintln!(
            "[MANUAL_MEMO] changes={} queued_markers={} start={} finish={}",
            changes.len(),
            queued_inserts.len(),
            start_count,
            finish_count
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: errors[0].message.clone(),
            diagnostics: errors,
        }))
    }
}

fn find_optional_places(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut optional_places: HashSet<IdentifierId> = HashSet::new();
    let blocks_by_id: HashMap<BlockId, &BasicBlock> = func
        .body
        .blocks
        .iter()
        .map(|(id, block)| (*id, block))
        .collect();

    for (_, block) in &func.body.blocks {
        let Terminal::Optional {
            optional: true,
            test: optional_test,
            fallthrough: optional_fallthrough,
            ..
        } = &block.terminal
        else {
            continue;
        };

        let mut test_block_id = *optional_test;
        'scan: while let Some(test_block) = blocks_by_id.get(&test_block_id).copied() {
            match &test_block.terminal {
                Terminal::Branch {
                    consequent,
                    fallthrough,
                    ..
                } => {
                    if *fallthrough == *optional_fallthrough {
                        if let Some(consequent_block) = blocks_by_id.get(consequent).copied()
                            && let Some(last) = consequent_block.instructions.last()
                            && let InstructionValue::StoreLocal { value, .. } = &last.value
                        {
                            optional_places.insert(value.identifier.id);
                        }
                        break 'scan;
                    }
                    test_block_id = *fallthrough;
                }
                Terminal::Optional { fallthrough, .. }
                | Terminal::Logical { fallthrough, .. }
                | Terminal::Sequence { fallthrough, .. }
                | Terminal::Ternary { fallthrough, .. } => {
                    test_block_id = *fallthrough;
                }
                _ => break 'scan,
            }
        }
    }

    optional_places
}

fn is_hook_alias_name(name: &str, hook_name: &str) -> bool {
    if name == hook_name {
        return true;
    }
    for segment in name.split('$') {
        if segment == hook_name {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MemoKind {
    UseMemo,
    UseCallback,
}

enum MemoTransform {
    /// useMemo(fn, deps) → CallExpression(fn, [])
    UseMemoFnRef { callee: Place },
    /// useCallback(fn, deps) → just load fn directly
    UseCallbackFnRef { callback: Place },
}

fn manual_memo_dependency_eq(a: &ManualMemoDependency, b: &ManualMemoDependency) -> bool {
    if !manual_memo_root_eq(&a.root, &b.root) || a.path.len() != b.path.len() {
        return false;
    }
    a.path
        .iter()
        .zip(&b.path)
        .all(|(ap, bp)| ap.property == bp.property && ap.optional == bp.optional)
}

fn manual_memo_root_eq(a: &ManualMemoRoot, b: &ManualMemoRoot) -> bool {
    match (a, b) {
        (
            ManualMemoRoot::Global {
                identifier_name: a_name,
            },
            ManualMemoRoot::Global {
                identifier_name: b_name,
            },
        ) => a_name == b_name,
        (ManualMemoRoot::NamedLocal(a_place), ManualMemoRoot::NamedLocal(b_place)) => {
            a_place.identifier.id == b_place.identifier.id
        }
        _ => false,
    }
}

fn dep_matches_store_target(
    dep: &ManualMemoDependency,
    branch_id: IdentifierId,
    store_local_targets: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    let Some(target_id) = store_local_targets.get(&branch_id).copied() else {
        return false;
    };
    matches!(
        &dep.root,
        ManualMemoRoot::NamedLocal(place) if place.identifier.id == target_id
    )
}

fn extract_deps_list(
    kind: MemoKind,
    args: &[Argument],
    maybe_deps_lists: &HashMap<IdentifierId, Vec<Place>>,
    maybe_deps: &HashMap<IdentifierId, ManualMemoDependency>,
    dep_defs: &HashMap<IdentifierId, String>,
    debug_manual_memo_deps: bool,
    errors: &mut Vec<CompilerDiagnostic>,
) -> Option<Vec<ManualMemoDependency>> {
    let kind_name = match kind {
        MemoKind::UseMemo => "useMemo",
        MemoKind::UseCallback => "useCallback",
    };

    // The second argument is the deps array
    let deps_place = match args.get(1) {
        Some(Argument::Place(place)) => place,
        Some(Argument::Spread(_)) => {
            errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: format!("Unexpected spread argument to {kind_name}"),
            });
            return None;
        }
        _ => return None,
    };

    let deps_list = match maybe_deps_lists.get(&deps_place.identifier.id) {
        Some(list) => list,
        None => {
            errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: format!(
                    "Expected the dependency list for {kind_name} to be an array literal"
                ),
            });
            return None;
        }
    };

    let mut result = Vec::new();
    for dep in deps_list {
        if let Some(maybe_dep) = maybe_deps.get(&dep.identifier.id) {
            result.push(maybe_dep.clone());
        } else {
            if debug_manual_memo_deps {
                let dep_name = dep
                    .identifier
                    .name
                    .as_ref()
                    .map(|name| format!("{:?}", name))
                    .unwrap_or_else(|| "<tmp>".to_string());
                let dep_def = dep_defs
                    .get(&dep.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| "<unknown-def>".to_string());
                eprintln!(
                    "[MANUAL_MEMO_DEPS] unresolved kind={} dep_id={} dep_name={} dep_def={}",
                    kind_name, dep.identifier.id.0, dep_name, dep_def
                );
            }
            errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: "Expected the dependency list to be an array of simple expressions (e.g. `x`, `x.y.z`, `x?.y?.z`)".to_string(),
            });
        }
    }
    Some(result)
}

/// Create a minimal temporary identifier for marker instructions.
fn make_temporary_identifier() -> Place {
    static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(900_000);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

fn apply_transform(instructions: &mut [Instruction], idx: usize, transform: &MemoTransform) {
    let loc = instructions[idx].loc.clone();
    let lvalue = instructions[idx].lvalue.clone();
    let id = instructions[idx].id;

    match transform {
        MemoTransform::UseMemoFnRef { callee } => {
            // Replace useMemo(fn, deps) with fn()
            instructions[idx] = Instruction {
                id,
                lvalue,
                value: InstructionValue::CallExpression {
                    callee: callee.clone(),
                    args: Vec::new(),
                    optional: false,
                    loc: loc.clone(),
                },
                loc,
                effects: None,
            };
        }
        MemoTransform::UseCallbackFnRef { callback } => {
            // Replace useCallback(fn, deps) with just loading fn
            instructions[idx] = Instruction {
                id,
                lvalue,
                value: InstructionValue::LoadLocal {
                    place: callback.clone(),
                    loc: loc.clone(),
                },
                loc,
                effects: None,
            };
        }
    }
}
