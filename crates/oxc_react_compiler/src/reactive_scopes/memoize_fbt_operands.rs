//! Port of MemoizeFbtAndMacroOperandsInSameScope.ts from upstream React Compiler.
//!
//! This pass supports the `fbt` translation system (https://facebook.github.io/fbt/)
//! as well as similar user-configurable macro-like APIs where it's important that
//! the name of the function not be changed, and its literal arguments not be turned
//! into temporaries.
//!
//! To ensure that the compiler doesn't rewrite code to violate fbt restrictions, we
//! force operands to fbt tags/calls to have the same scope as the tag/call itself.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;
use crate::options::MacroProp;

const FBT_TAGS: &[&str] = &["fbt", "fbt:param", "fbs", "fbs:param"];

#[derive(Debug, Clone)]
enum MacroTag {
    Simple(String),
    WithMethods(String, Vec<Vec<MacroMethodSegment>>),
}

#[derive(Debug, Clone)]
enum MacroMethodSegment {
    Wildcard,
    Name(String),
}

struct ScopeAction {
    block_idx: usize,
    instr_idx: usize,
    add_to_fbt_values: bool,
    skip_named: bool,
}

fn upsert_scope_assignment(
    map: &mut HashMap<IdentifierId, ReactiveScope>,
    id: IdentifierId,
    new_scope: &ReactiveScope,
) {
    use std::collections::hash_map::Entry;
    match map.entry(id) {
        Entry::Vacant(v) => {
            v.insert(new_scope.clone());
        }
        Entry::Occupied(mut o) => {
            let existing = o.get_mut();
            if existing.id == new_scope.id {
                if new_scope.range.start < existing.range.start {
                    existing.range.start = new_scope.range.start;
                }
                if new_scope.range.end > existing.range.end {
                    existing.range.end = new_scope.range.end;
                }
            } else {
                // Preserve last-write behavior for conflicting scope ids.
                *existing = new_scope.clone();
            }
        }
    }
}

pub fn memoize_fbt_and_macro_operands_in_same_scope(
    func: &mut HIRFunction,
) -> HashSet<IdentifierId> {
    let debug_fbt = std::env::var("DEBUG_FBT_OPERANDS").is_ok();
    let mut fbt_macro_tags: Vec<MacroTag> = FBT_TAGS
        .iter()
        .map(|tag| MacroTag::Simple(tag.to_string()))
        .collect();

    if let Some(custom_macros) = &func.env.config().custom_macros {
        for cm in custom_macros {
            if cm.props.is_empty() {
                fbt_macro_tags.push(MacroTag::Simple(cm.name.clone()));
            } else {
                let methods: Vec<MacroMethodSegment> = cm
                    .props
                    .iter()
                    .map(|p| match p {
                        MacroProp::Name(n) => MacroMethodSegment::Name(n.clone()),
                        MacroProp::Wildcard => MacroMethodSegment::Wildcard,
                    })
                    .collect();
                fbt_macro_tags.push(MacroTag::WithMethods(cm.name.clone(), vec![methods]));
            }
        }
    }

    if debug_fbt {
        eprintln!(
            "[FBT_OPERANDS] fn={} custom_macros={:?} tags={:?}",
            func.id.as_deref().unwrap_or("<anonymous>"),
            func.env.config().custom_macros,
            fbt_macro_tags
        );
    }

    let mut fbt_values: HashSet<IdentifierId> = HashSet::new();
    let mut macro_methods: HashMap<IdentifierId, Vec<Vec<MacroMethodSegment>>> = HashMap::new();
    let mut iteration = 0usize;

    loop {
        iteration += 1;
        let vsize = fbt_values.len();
        let msize = macro_methods.len();
        visit(func, &fbt_macro_tags, &mut fbt_values, &mut macro_methods);
        if debug_fbt {
            eprintln!(
                "[FBT_OPERANDS] iter={} fbt_values={} macro_methods={}",
                iteration,
                fbt_values.len(),
                macro_methods.len()
            );
        }
        if fbt_values.len() == vsize && macro_methods.len() == msize {
            break;
        }
    }

    fbt_values
}

fn visit(
    func: &mut HIRFunction,
    fbt_macro_tags: &[MacroTag],
    fbt_values: &mut HashSet<IdentifierId>,
    macro_methods: &mut HashMap<IdentifierId, Vec<Vec<MacroMethodSegment>>>,
) {
    let debug_fbt = std::env::var("DEBUG_FBT_OPERANDS").is_ok();
    let mut load_global_ids: HashSet<IdentifierId> = HashSet::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            if matches!(instr.value, InstructionValue::LoadGlobal { .. }) {
                load_global_ids.insert(instr.lvalue.identifier.id);
            }
        }
    }

    let mut def_kind_by_id: HashMap<IdentifierId, &'static str> = HashMap::new();
    if debug_fbt {
        for (_block_id, block) in &func.body.blocks {
            for instr in &block.instructions {
                let kind = match &instr.value {
                    InstructionValue::LoadGlobal { .. } => "LoadGlobal",
                    InstructionValue::LoadLocal { .. } => "LoadLocal",
                    InstructionValue::LoadContext { .. } => "LoadContext",
                    InstructionValue::StoreLocal { .. } => "StoreLocal",
                    InstructionValue::StoreContext { .. } => "StoreContext",
                    InstructionValue::Primitive { .. } => "Primitive",
                    InstructionValue::CallExpression { .. } => "CallExpression",
                    InstructionValue::MethodCall { .. } => "MethodCall",
                    InstructionValue::ArrayExpression { .. } => "ArrayExpression",
                    InstructionValue::ObjectExpression { .. } => "ObjectExpression",
                    InstructionValue::PropertyLoad { .. } => "PropertyLoad",
                    InstructionValue::Destructure { .. } => "Destructure",
                    _ => "Other",
                };
                def_kind_by_id.insert(instr.lvalue.identifier.id, kind);
            }
        }
    }
    let mut actions: Vec<ScopeAction> = Vec::new();

    'outer: for (block_idx, (_block_id, block)) in func.body.blocks.iter().enumerate() {
        for (instr_idx, instr) in block.instructions.iter().enumerate() {
            let lvalue_id = instr.lvalue.identifier.id;

            let mut handled_tag_match = false;
            match &instr.value {
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(s),
                    ..
                } => {
                    if matches_exact_tag(s, fbt_macro_tags) {
                        fbt_values.insert(lvalue_id);
                        if debug_fbt {
                            eprintln!(
                                "[FBT_OPERANDS] primitive-match bb={} instr#{} lvalue={}",
                                block_idx, instr.id.0, lvalue_id.0
                            );
                        }
                        handled_tag_match = true;
                    }
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    let name = binding.name();
                    if matches_exact_tag(name, fbt_macro_tags) {
                        fbt_values.insert(lvalue_id);
                        if debug_fbt {
                            eprintln!(
                                "[FBT_OPERANDS] global-match bb={} instr#{} lvalue={} name={}",
                                block_idx, instr.id.0, lvalue_id.0, name
                            );
                        }
                        handled_tag_match = true;
                    } else if let Some(methods) = match_tag_root(name, fbt_macro_tags) {
                        macro_methods.insert(lvalue_id, methods);
                        if debug_fbt {
                            eprintln!(
                                "[FBT_OPERANDS] global-root bb={} instr#{} lvalue={} name={} methods={:?}",
                                block_idx,
                                instr.id.0,
                                lvalue_id.0,
                                name,
                                macro_methods.get(&lvalue_id)
                            );
                        }
                        handled_tag_match = true;
                    }
                }
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if let Some(methods) = macro_methods.get(&object.identifier.id).cloned() {
                        let prop_name = match property {
                            PropertyLiteral::String(s) => Some(s.as_str()),
                            PropertyLiteral::Number(_) => None,
                        };
                        let mut new_methods = Vec::new();
                        for method in &methods {
                            if !method.is_empty() {
                                let matches = match &method[0] {
                                    MacroMethodSegment::Wildcard => true,
                                    MacroMethodSegment::Name(n) => {
                                        prop_name.is_some_and(|p| p == n)
                                    }
                                };
                                if matches {
                                    if method.len() > 1 {
                                        new_methods.push(method[1..].to_vec());
                                    } else {
                                        fbt_values.insert(lvalue_id);
                                        if debug_fbt {
                                            eprintln!(
                                                "[FBT_OPERANDS] property-final bb={} instr#{} lvalue={} object={} property={:?}",
                                                block_idx,
                                                instr.id.0,
                                                lvalue_id.0,
                                                object.identifier.id.0,
                                                property
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        if !new_methods.is_empty() {
                            macro_methods.insert(lvalue_id, new_methods);
                            if debug_fbt {
                                eprintln!(
                                    "[FBT_OPERANDS] property-progress bb={} instr#{} lvalue={} object={} methods={:?}",
                                    block_idx,
                                    instr.id.0,
                                    lvalue_id.0,
                                    object.identifier.id.0,
                                    macro_methods.get(&lvalue_id)
                                );
                            }
                        }
                        handled_tag_match = true;
                    }
                }
                _ => {}
            }

            // Match upstream's else-if structure: once an instruction is classified as
            // a macro tag/root/property progression, do not run the downstream branches.
            if handled_tag_match {
                continue;
            }

            if is_fbt_call_expression(fbt_values, &instr.value) {
                if debug_fbt {
                    eprintln!(
                        "[FBT_OPERANDS] call bb={} instr#{} lvalue={} has_scope={}",
                        block_idx,
                        instr.id.0,
                        lvalue_id.0,
                        instr.lvalue.identifier.scope.is_some()
                    );
                }
                if instr.lvalue.identifier.scope.is_some() {
                    actions.push(ScopeAction {
                        block_idx,
                        instr_idx,
                        add_to_fbt_values: true,
                        skip_named: false,
                    });
                }
            } else if is_fbt_jsx_expression(fbt_macro_tags, fbt_values, &instr.value)
                || is_fbt_jsx_child(fbt_values, lvalue_id, &instr.value)
            {
                if debug_fbt {
                    eprintln!(
                        "[FBT_OPERANDS] jsx bb={} instr#{} lvalue={} has_scope={}",
                        block_idx,
                        instr.id.0,
                        lvalue_id.0,
                        instr.lvalue.identifier.scope.is_some()
                    );
                }
                if instr.lvalue.identifier.scope.is_some() {
                    actions.push(ScopeAction {
                        block_idx,
                        instr_idx,
                        add_to_fbt_values: true,
                        skip_named: false,
                    });
                }
            } else if fbt_values.contains(&lvalue_id) {
                if instr.lvalue.identifier.scope.is_none() {
                    if debug_fbt {
                        eprintln!(
                            "[FBT_OPERANDS] early-return-null-scope bb={} instr#{} lvalue={}",
                            block_idx, instr.id.0, lvalue_id.0
                        );
                    }
                    break 'outer;
                }
                actions.push(ScopeAction {
                    block_idx,
                    instr_idx,
                    add_to_fbt_values: false,
                    skip_named: true,
                });
            }
        }
    }

    // Upstream mutates shared Identifier objects; our HIR stores copied identifiers
    // in each occurrence. Track per-id scope assignments and apply them globally.
    let mut scope_assignments: HashMap<IdentifierId, ReactiveScope> = HashMap::new();

    for action in &actions {
        let instr = &func.body.blocks[action.block_idx].1.instructions[action.instr_idx];
        let fbt_lvalue_id = instr.lvalue.identifier.id;
        let fbt_scope = match &instr.lvalue.identifier.scope {
            Some(scope) => (**scope).clone(),
            None => continue,
        };

        let mut named_ids: HashSet<IdentifierId> = HashSet::new();
        let mut operand_ids: Vec<IdentifierId> = Vec::new();
        let instr_id = instr.id.0;
        let initial_scope_start = instr
            .lvalue
            .identifier
            .scope
            .as_ref()
            .map_or(0, |s| s.range.start.0);
        let mut min_start = fbt_scope.range.start;

        crate::hir::visitors::for_each_instruction_operand(instr, |place| {
            operand_ids.push(place.identifier.id);
            let ms = place.identifier.mutable_range.start;
            if debug_fbt {
                let def_kind = def_kind_by_id
                    .get(&place.identifier.id)
                    .copied()
                    .unwrap_or("?");
                eprintln!(
                    "[FBT_OPERANDS] operand instr#{} id={} def_kind={} decl={} name={} mutable_start={}",
                    instr.id.0,
                    place.identifier.id.0,
                    def_kind,
                    place.identifier.declaration_id.0,
                    place
                        .identifier
                        .name
                        .as_ref()
                        .map_or("<unnamed>", |n| n.value()),
                    ms.0
                );
            }
            if ms.0 != 0 && ms < min_start {
                min_start = ms;
            }
            if action.skip_named
                && let Some(IdentifierName::Named(_)) = &place.identifier.name
            {
                named_ids.insert(place.identifier.id);
            }
        });

        let operand_id_set: HashSet<IdentifierId> = operand_ids.iter().copied().collect();

        let instr_mut = &mut func.body.blocks[action.block_idx].1.instructions[action.instr_idx];
        if let Some(scope) = &mut instr_mut.lvalue.identifier.scope {
            scope.range.start = min_start;
        }

        let mut scope_to_assign = fbt_scope;
        scope_to_assign.range.start = min_start;
        upsert_scope_assignment(&mut scope_assignments, fbt_lvalue_id, &scope_to_assign);
        if debug_fbt {
            eprintln!(
                "[FBT_OPERANDS] action instr#{} lvalue={} start={} -> min_start={} add_to_fbt={} skip_named={}",
                instr_id,
                fbt_lvalue_id.0,
                initial_scope_start,
                min_start.0,
                action.add_to_fbt_values,
                action.skip_named
            );
        }

        crate::hir::visitors::map_instruction_operands(instr_mut, |place| {
            if !operand_id_set.contains(&place.identifier.id) {
                return;
            }
            if action.skip_named && named_ids.contains(&place.identifier.id) {
                return;
            }
            place.identifier.scope = Some(Box::new(scope_to_assign.clone()));
            upsert_scope_assignment(
                &mut scope_assignments,
                place.identifier.id,
                &scope_to_assign,
            );
        });

        if action.add_to_fbt_values {
            for id in &operand_ids {
                fbt_values.insert(*id);
                if debug_fbt {
                    eprintln!(
                        "[FBT_OPERANDS] add-operand instr#{} operand={}",
                        instr_mut.id.0, id.0
                    );
                }
            }
        }
    }

    if !scope_assignments.is_empty() {
        for (_block_id, block) in &mut func.body.blocks {
            for instr in &mut block.instructions {
                crate::hir::visitors::map_instruction_lvalues(instr, |place| {
                    if let Some(scope) = scope_assignments.get(&place.identifier.id) {
                        place.identifier.scope = Some(Box::new(scope.clone()));
                    }
                });
                crate::hir::visitors::map_instruction_operands(instr, |place| {
                    if let Some(scope) = scope_assignments.get(&place.identifier.id) {
                        place.identifier.scope = Some(Box::new(scope.clone()));
                    }
                });
            }
            crate::hir::visitors::map_terminal_operands(&mut block.terminal, |place| {
                if let Some(scope) = scope_assignments.get(&place.identifier.id) {
                    place.identifier.scope = Some(Box::new(scope.clone()));
                }
            });
        }
    }
}

fn matches_exact_tag(s: &str, tags: &[MacroTag]) -> bool {
    tags.iter().any(|m| match m {
        MacroTag::Simple(tag) => s == tag,
        MacroTag::WithMethods(tag, methods) => methods.is_empty() && s == tag,
    })
}

fn match_tag_root(s: &str, tags: &[MacroTag]) -> Option<Vec<Vec<MacroMethodSegment>>> {
    let mut methods = Vec::new();
    for m in tags {
        if let MacroTag::WithMethods(tag, method_paths) = m
            && tag == s
            && !method_paths.is_empty()
        {
            methods.extend(method_paths.iter().cloned());
        }
    }
    if methods.is_empty() {
        None
    } else {
        Some(methods)
    }
}

fn is_fbt_call_expression(fbt_values: &HashSet<IdentifierId>, value: &InstructionValue) -> bool {
    match value {
        InstructionValue::CallExpression { callee, .. } => {
            fbt_values.contains(&callee.identifier.id)
        }
        InstructionValue::MethodCall { property, .. } => {
            fbt_values.contains(&property.identifier.id)
        }
        _ => false,
    }
}

fn is_fbt_jsx_expression(
    fbt_macro_tags: &[MacroTag],
    fbt_values: &HashSet<IdentifierId>,
    value: &InstructionValue,
) -> bool {
    match value {
        InstructionValue::JsxExpression { tag, .. } => match tag {
            JsxTag::Component(place) => fbt_values.contains(&place.identifier.id),
            JsxTag::BuiltinTag(name) => matches_exact_tag(name, fbt_macro_tags),
            JsxTag::Fragment => false,
        },
        _ => false,
    }
}

fn is_fbt_jsx_child(
    fbt_values: &HashSet<IdentifierId>,
    lvalue_id: IdentifierId,
    value: &InstructionValue,
) -> bool {
    matches!(
        value,
        InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
    ) && fbt_values.contains(&lvalue_id)
}
