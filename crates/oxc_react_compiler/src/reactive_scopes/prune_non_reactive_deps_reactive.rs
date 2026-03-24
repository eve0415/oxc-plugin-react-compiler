//! Prune non-reactive dependencies from reactive scopes (ReactiveFunction version).
//!
//! Port of `PruneNonReactiveDependencies.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This is the ReactiveFunction-level version of `prune_non_reactive_deps` (which
//! operates on the HIR). It collects reactive identifiers by walking the tree,
//! then removes scope dependencies that are not reactive.
//!
//! After pruning, if a scope still has reactive dependencies, all of its
//! declarations and reassignments are marked as reactive (since they may
//! re-evaluate when reactive inputs change).

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

/// Prune non-reactive dependencies from all reactive scopes in a ReactiveFunction.
///
/// 1. Collects reactive identifiers (via `collect_reactive_identifiers`).
/// 2. Walks the reactive tree, propagating reactivity through data-flow instructions.
/// 3. For each scope, removes dependencies whose identifier is not reactive.
/// 4. If a scope still has reactive deps after pruning, marks all its declarations
///    and reassignments as reactive.
pub fn prune_non_reactive_deps_reactive(func: &mut ReactiveFunction) {
    let reassigned_decls = collect_reassigned_decl_ids(func);
    let original_named_ids = collect_original_named_identifier_ids(func);
    let stable_pruned_function_aliases = collect_stable_pruned_function_aliases(func);
    let (mut reactive_ids, mut reactive_decls) = collect_reactive_identifiers(func);
    propagate_reactivity_to_fixpoint(&func.body, &mut reactive_ids, &mut reactive_decls);
    if std::env::var("DEBUG_REACTIVE_IDS").is_ok() {
        let mut ids: Vec<u32> = reactive_ids.iter().map(|id| id.0).collect();
        ids.sort_unstable();
        eprintln!("[REACTIVE_IDS] initial={:?}", ids);
    }
    visit_and_prune_block(
        &mut func.body,
        &mut reactive_ids,
        &mut reactive_decls,
        &reassigned_decls,
        &original_named_ids,
        &stable_pruned_function_aliases,
    );
    if std::env::var("DEBUG_REACTIVE_IDS").is_ok() {
        let mut ids: Vec<u32> = reactive_ids.iter().map(|id| id.0).collect();
        ids.sort_unstable();
        eprintln!("[REACTIVE_IDS] final={:?}", ids);
    }
}

/// Run reactivity propagation over the full reactive tree until no new reactive
/// identifiers are discovered.
fn propagate_reactivity_to_fixpoint(
    body: &ReactiveBlock,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    loop {
        let before_ids = reactive_ids.len();
        let before_decls = reactive_decls.len();
        propagate_reactivity_block(body, reactive_ids, reactive_decls);
        if reactive_ids.len() == before_ids && reactive_decls.len() == before_decls {
            break;
        }
    }
}

fn propagate_reactivity_block(
    block: &ReactiveBlock,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                propagate_reactivity(instr, reactive_ids, reactive_decls)
            }
            ReactiveStatement::Terminal(term_stmt) => {
                propagate_reactivity_terminal(&term_stmt.terminal, reactive_ids, reactive_decls)
            }
            ReactiveStatement::Scope(scope_block) => {
                propagate_reactivity_block(&scope_block.instructions, reactive_ids, reactive_decls)
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                propagate_reactivity_block(&scope_block.instructions, reactive_ids, reactive_decls)
            }
        }
    }
}

fn propagate_reactivity_terminal(
    terminal: &ReactiveTerminal,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            propagate_reactivity_block(consequent, reactive_ids, reactive_decls);
            if let Some(alt) = alternate {
                propagate_reactivity_block(alt, reactive_ids, reactive_decls);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    propagate_reactivity_block(block, reactive_ids, reactive_decls);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            propagate_reactivity_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            propagate_reactivity_block(init, reactive_ids, reactive_decls);
            if let Some(upd) = update {
                propagate_reactivity_block(upd, reactive_ids, reactive_decls);
            }
            propagate_reactivity_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            propagate_reactivity_block(init, reactive_ids, reactive_decls);
            propagate_reactivity_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Label { block, .. } => {
            propagate_reactivity_block(block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            propagate_reactivity_block(block, reactive_ids, reactive_decls);
            propagate_reactivity_block(handler, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn mark_reactive_identifier(
    reason: &str,
    identifier: &Identifier,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) -> bool {
    let inserted_id = reactive_ids.insert(identifier.id);
    let inserted_decl = reactive_decls.insert(identifier.declaration_id);
    if (inserted_id || inserted_decl) && std::env::var("DEBUG_PRUNE_REACTIVE_MARKS").is_ok() {
        eprintln!(
            "[PRUNE_REACTIVE_MARK] reason={} id={} decl={} name={}",
            reason,
            identifier.id.0,
            identifier.declaration_id.0,
            identifier
                .name
                .as_ref()
                .map(|name| name.value().to_string())
                .unwrap_or_else(|| "<unnamed>".to_string())
        );
    }
    inserted_id || inserted_decl
}

fn is_identifier_reactive(
    identifier: &Identifier,
    reactive_ids: &HashSet<IdentifierId>,
    reactive_decls: &HashSet<DeclarationId>,
) -> bool {
    let _ = reactive_decls;
    reactive_ids.contains(&identifier.id)
}

/// Collect all reactive identifier IDs by walking the reactive function tree.
///
/// Port of `CollectReactiveIdentifiers.ts`.
/// Seeds from `place.reactive` flags set by `InferReactivePlaces`, then walks
/// all places (including lvalues) to collect reactive identifiers.
/// Also handles PrunedScope blocks (which mark non-primitive declarations as reactive).
fn collect_reactive_identifiers(
    func: &ReactiveFunction,
) -> (HashSet<IdentifierId>, HashSet<DeclarationId>) {
    let mut reactive_ids: HashSet<IdentifierId> = HashSet::new();
    let mut reactive_decls: HashSet<DeclarationId> = HashSet::new();
    // Function inputs are reactive by definition for memo invalidation.
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                mark_reactive_identifier(
                    "param",
                    &p.identifier,
                    &mut reactive_ids,
                    &mut reactive_decls,
                );
            }
        }
    }
    collect_from_block(&func.body, &mut reactive_ids, &mut reactive_decls);
    (reactive_ids, reactive_decls)
}

fn collect_from_block(
    block: &ReactiveBlock,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                collect_from_instruction(instr, reactive_ids, reactive_decls);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_from_terminal(&term_stmt.terminal, reactive_ids, reactive_decls);
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_from_block(&scope_block.instructions, reactive_ids, reactive_decls);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_from_block(&scope_block.instructions, reactive_ids, reactive_decls);
                let named_function_decl_ids =
                    collect_named_function_decl_ids(&scope_block.instructions);
                for (id, decl) in &scope_block.scope.declarations {
                    if !is_primitive_type(&decl.identifier)
                        && !named_function_decl_ids.contains(&decl.identifier.declaration_id)
                        && !is_stable_ref_type(&decl.identifier, reactive_ids, reactive_decls)
                    {
                        reactive_ids.insert(*id);
                        reactive_decls.insert(decl.identifier.declaration_id);
                        if std::env::var("DEBUG_PRUNE_REACTIVE_MARKS").is_ok() {
                            eprintln!(
                                "[PRUNE_REACTIVE_MARK] reason=collect:pruned_scope_decl id={} decl={} name={}",
                                id.0,
                                decl.identifier.declaration_id.0,
                                decl.identifier
                                    .name
                                    .as_ref()
                                    .map(|name| name.value().to_string())
                                    .unwrap_or_else(|| "<unnamed>".to_string())
                            );
                        }
                    }
                }
            }
        }
    }
}

fn collect_from_instruction(
    instr: &ReactiveInstruction,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    // Visit lvalue as a place (upstream visitLValue delegates to visitPlace)
    if let Some(lvalue) = &instr.lvalue
        && lvalue.reactive
    {
        mark_reactive_identifier(
            "collect:lvalue",
            &lvalue.identifier,
            reactive_ids,
            reactive_decls,
        );
    }

    // Visit all operand places
    visit_flagged_reactive_places_in_value(&instr.value, reactive_ids, reactive_decls);
}

/// Visit all Place references in an instruction value and add reactive ones to the set.
fn visit_flagged_reactive_places_in_value(
    value: &InstructionValue,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    let mut visit = |place: &Place| {
        if place.reactive {
            mark_reactive_identifier(
                "collect:operand",
                &place.identifier,
                reactive_ids,
                reactive_decls,
            );
        }
    };
    visit_each_place_in_value(value, &mut visit);
}

/// Visit all Place references in an instruction value.
fn visit_each_place_in_value<F: FnMut(&Place)>(value: &InstructionValue, visit: &mut F) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            visit(place);
        }
        InstructionValue::StoreLocal {
            lvalue, value: val, ..
        }
        | InstructionValue::StoreContext {
            lvalue, value: val, ..
        } => {
            visit(&lvalue.place);
            visit(val);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            visit(&lvalue.place);
        }
        InstructionValue::Destructure {
            lvalue, value: val, ..
        } => {
            visit(val);
            for_each_pattern_place(&lvalue.pattern, &mut |place| visit(place));
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            visit(left);
            visit(right);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            visit(val);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            visit(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => visit(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            visit(receiver);
            visit(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => visit(p),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            visit(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => visit(p),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            visit(place);
                        }
                        visit(&p.place);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        visit(place);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        visit(place);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            visit(object);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            visit(object);
            visit(val);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            visit(object);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            visit(object);
            visit(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            visit(object);
            visit(property);
            visit(val);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            visit(object);
            visit(property);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            visit(val);
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(place) = tag {
                visit(place);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => visit(place),
                    JsxAttribute::SpreadAttribute { argument } => visit(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    visit(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                visit(child);
            }
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            visit(test);
            visit(consequent);
            visit(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            visit(left);
            visit(right);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    visit(lvalue);
                }
                visit_each_place_in_value(&instr.value, visit);
            }
            visit_each_place_in_value(value, visit);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            visit_each_place_in_value(value, visit);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            visit_each_place_in_value(left, visit);
            visit_each_place_in_value(right, visit);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            visit(tag);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                visit(expr);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            visit(val);
        }
        InstructionValue::GetIterator { collection, .. } => {
            visit(collection);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            visit(iterator);
            visit(collection);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            visit(val);
        }
        InstructionValue::PrefixUpdate {
            lvalue, value: val, ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue, value: val, ..
        } => {
            visit(lvalue);
            visit(val);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            visit(decl);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            visit(val);
        }
        InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn collect_from_terminal(
    terminal: &ReactiveTerminal,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    let visit = |place: &Place,
                 reactive_ids: &mut HashSet<IdentifierId>,
                 reactive_decls: &mut HashSet<DeclarationId>| {
        if place.reactive {
            mark_reactive_identifier(
                "terminal:operand",
                &place.identifier,
                reactive_ids,
                reactive_decls,
            );
        }
    };

    match terminal {
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            visit(value, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            visit(test, reactive_ids, reactive_decls);
            collect_from_block(consequent, reactive_ids, reactive_decls);
            if let Some(alt) = alternate {
                collect_from_block(alt, reactive_ids, reactive_decls);
            }
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            visit(test, reactive_ids, reactive_decls);
            for case in cases {
                if let Some(t) = &case.test {
                    visit(t, reactive_ids, reactive_decls);
                }
                if let Some(block) = &case.block {
                    collect_from_block(block, reactive_ids, reactive_decls);
                }
            }
        }
        ReactiveTerminal::DoWhile {
            loop_block, test, ..
        } => {
            collect_from_block(loop_block, reactive_ids, reactive_decls);
            visit(test, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            visit(test, reactive_ids, reactive_decls);
            collect_from_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            collect_from_block(init, reactive_ids, reactive_decls);
            visit(test, reactive_ids, reactive_decls);
            if let Some(upd) = update {
                collect_from_block(upd, reactive_ids, reactive_decls);
            }
            collect_from_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            collect_from_block(init, reactive_ids, reactive_decls);
            visit(test, reactive_ids, reactive_decls);
            collect_from_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_from_block(init, reactive_ids, reactive_decls);
            collect_from_block(loop_block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_from_block(block, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            collect_from_block(block, reactive_ids, reactive_decls);
            if let Some(binding) = handler_binding {
                visit(binding, reactive_ids, reactive_decls);
            }
            collect_from_block(handler, reactive_ids, reactive_decls);
        }
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
    }
}

/// Helper: iterate over all Place references in a pattern.
fn for_each_pattern_place<F: FnMut(&Place)>(pattern: &Pattern, f: &mut F) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => f(place),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => f(&p.place),
                    ObjectPropertyOrSpread::Spread(place) => f(place),
                }
            }
        }
    }
}

/// Check if an identifier has a primitive type.
fn is_primitive_type(id: &Identifier) -> bool {
    matches!(id.type_, Type::Primitive)
}

fn is_plain_function_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn collect_named_function_decl_ids(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut out = HashSet::new();
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(lvalue) = &instr.lvalue {
                    match &instr.value {
                        InstructionValue::FunctionExpression { lowered_func, .. }
                        | InstructionValue::ObjectMethod { lowered_func, .. } => {
                            if lowered_func
                                .func
                                .id
                                .as_deref()
                                .is_some_and(is_plain_function_name)
                            {
                                out.insert(lvalue.identifier.declaration_id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                out.extend(collect_named_function_decl_ids_from_terminal(
                    &term_stmt.terminal,
                ));
            }
            ReactiveStatement::Scope(scope_block) => {
                out.extend(collect_named_function_decl_ids(&scope_block.instructions));
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                out.extend(collect_named_function_decl_ids(&scope_block.instructions));
            }
        }
    }
    out
}

fn collect_named_function_decl_ids_from_terminal(
    terminal: &ReactiveTerminal,
) -> HashSet<DeclarationId> {
    let mut out = HashSet::new();
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            out.extend(collect_named_function_decl_ids(consequent));
            if let Some(alt) = alternate {
                out.extend(collect_named_function_decl_ids(alt));
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    out.extend(collect_named_function_decl_ids(block));
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            out.extend(collect_named_function_decl_ids(loop_block));
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            out.extend(collect_named_function_decl_ids(init));
            if let Some(update) = update {
                out.extend(collect_named_function_decl_ids(update));
            }
            out.extend(collect_named_function_decl_ids(loop_block));
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            out.extend(collect_named_function_decl_ids(init));
            out.extend(collect_named_function_decl_ids(loop_block));
        }
        ReactiveTerminal::Label { block, .. } => {
            out.extend(collect_named_function_decl_ids(block));
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            out.extend(collect_named_function_decl_ids(block));
            out.extend(collect_named_function_decl_ids(handler));
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
    out
}

/// Check if an identifier is a stable ref type (useRef that is not reactive).
fn is_stable_ref_type(
    id: &Identifier,
    reactive_ids: &HashSet<IdentifierId>,
    reactive_decls: &HashSet<DeclarationId>,
) -> bool {
    is_use_ref_type(id) && !is_identifier_reactive(id, reactive_ids, reactive_decls)
}

/// Check if an identifier is a useRef type.
fn is_use_ref_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(shape) } if shape == "BuiltInUseRefId")
}

/// Check if an identifier is a stable type (setState, dispatch, useRef, startTransition).
fn is_stable_type(id: &Identifier) -> bool {
    let shape = match &id.type_ {
        Type::Object {
            shape_id: Some(shape),
        } => Some(shape.as_str()),
        Type::Function {
            shape_id: Some(shape),
            ..
        } => Some(shape.as_str()),
        _ => None,
    };
    matches!(
        shape,
        Some(
            "BuiltInSetState"
                | "BuiltInSetActionState"
                | "BuiltInDispatch"
                | "BuiltInUseRefId"
                | "BuiltInStartTransition"
        )
    )
}

// -------------------------------------------------------------------------
// Phase 2: Walk the tree propagating reactivity and pruning deps
// -------------------------------------------------------------------------

fn visit_and_prune_block(
    block: &mut ReactiveBlock,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
    reassigned_decls: &HashSet<DeclarationId>,
    original_named_ids: &HashMap<DeclarationId, IdentifierId>,
    stable_pruned_function_aliases: &HashSet<DeclarationId>,
) {
    let debug_flow = std::env::var("DEBUG_PRUNE_NONREACTIVE_FLOW").is_ok();
    for i in 0..block.len() {
        let keep_nonreactive_reassigned_deps =
            should_retain_reassigned_deps_for_scope(&block[..i], reactive_ids, reactive_decls);
        match &mut block[i] {
            ReactiveStatement::Instruction(instr) => {
                propagate_reactivity(instr, reactive_ids, reactive_decls);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                visit_and_prune_terminal(
                    &mut term_stmt.terminal,
                    reactive_ids,
                    reactive_decls,
                    reassigned_decls,
                    original_named_ids,
                    stable_pruned_function_aliases,
                );
            }
            ReactiveStatement::Scope(scope_block) => {
                visit_and_prune_block(
                    &mut scope_block.instructions,
                    reactive_ids,
                    reactive_decls,
                    reassigned_decls,
                    original_named_ids,
                    stable_pruned_function_aliases,
                );

                if debug_flow {
                    eprintln!(
                        "[PRUNE_NONREACTIVE] scope={} keep_reassigned_fallback={} deps_before={:?}",
                        scope_block.scope.id.0,
                        keep_nonreactive_reassigned_deps,
                        scope_block
                            .scope
                            .dependencies
                            .iter()
                            .map(|dep| {
                                format!(
                                    "{}#{}:{} reactive_id={} reactive_decl={} path_len={}",
                                    dep.identifier.id.0,
                                    dep.identifier.declaration_id.0,
                                    dep.identifier
                                        .name
                                        .as_ref()
                                        .map(|name| name.value().to_string())
                                        .unwrap_or_else(|| "<unnamed>".to_string()),
                                    reactive_ids.contains(&dep.identifier.id),
                                    reactive_decls.contains(&dep.identifier.declaration_id),
                                    dep.path.len()
                                )
                            })
                            .collect::<Vec<_>>()
                    );
                }
                scope_block.scope.dependencies.retain(|dep| {
                    let keep = (!stable_pruned_function_aliases
                        .contains(&dep.identifier.declaration_id)
                        || !dep.path.is_empty())
                        && (is_identifier_reactive(&dep.identifier, reactive_ids, reactive_decls)
                        || (reactive_decls.contains(&dep.identifier.declaration_id)
                            && original_named_ids.get(&dep.identifier.declaration_id)
                                == Some(&dep.identifier.id))
                        || (keep_nonreactive_reassigned_deps
                            && dep.path.is_empty()
                            && reassigned_decls.contains(&dep.identifier.declaration_id)
                            && matches!(
                                dep.identifier.type_,
                                Type::Primitive | Type::TypeVar { .. }
                            )));
                    if debug_flow {
                        eprintln!(
                            "[PRUNE_NONREACTIVE] scope={} dep_id={} dep_decl={} name={} keep={} reactive_id={} reactive_decl={} reassigned={} path_len={}",
                            scope_block.scope.id.0,
                            dep.identifier.id.0,
                            dep.identifier.declaration_id.0,
                            dep.identifier
                                .name
                                .as_ref()
                                .map(|name| name.value().to_string())
                                .unwrap_or_else(|| "<unnamed>".to_string()),
                            keep,
                            reactive_ids.contains(&dep.identifier.id),
                            reactive_decls.contains(&dep.identifier.declaration_id),
                            reassigned_decls.contains(&dep.identifier.declaration_id),
                            dep.path.len()
                        );
                    }
                    keep
                });

                if !scope_block.scope.dependencies.is_empty() {
                    for decl in scope_block.scope.declarations.values() {
                        mark_reactive_identifier(
                            "scope:reactive_dep_decl",
                            &decl.identifier,
                            reactive_ids,
                            reactive_decls,
                        );
                    }
                    for reassignment in &scope_block.scope.reassignments {
                        mark_reactive_identifier(
                            "scope:reactive_dep_reassignment",
                            reassignment,
                            reactive_ids,
                            reactive_decls,
                        );
                    }
                }
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                visit_and_prune_block(
                    &mut scope_block.instructions,
                    reactive_ids,
                    reactive_decls,
                    reassigned_decls,
                    original_named_ids,
                    stable_pruned_function_aliases,
                );
            }
        }
    }
}

fn should_retain_reassigned_deps_for_scope(
    preceding: &[ReactiveStatement],
    reactive_ids: &HashSet<IdentifierId>,
    reactive_decls: &HashSet<DeclarationId>,
) -> bool {
    for stmt in preceding.iter().rev() {
        match stmt {
            ReactiveStatement::Instruction(_) => continue,
            ReactiveStatement::Terminal(term_stmt) => {
                return terminal_allows_reassigned_dep_fallback(
                    &term_stmt.terminal,
                    reactive_ids,
                    reactive_decls,
                );
            }
            ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_) => return false,
        }
    }
    false
}

fn terminal_allows_reassigned_dep_fallback(
    terminal: &ReactiveTerminal,
    reactive_ids: &HashSet<IdentifierId>,
    reactive_decls: &HashSet<DeclarationId>,
) -> bool {
    match terminal {
        ReactiveTerminal::DoWhile { test, .. }
        | ReactiveTerminal::While { test, .. }
        | ReactiveTerminal::For { test, .. }
        | ReactiveTerminal::ForOf { test, .. } => {
            is_identifier_reactive(&test.identifier, reactive_ids, reactive_decls)
        }
        ReactiveTerminal::ForIn { .. } => false,
        ReactiveTerminal::If { test, .. } => {
            is_identifier_reactive(&test.identifier, reactive_ids, reactive_decls)
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            is_identifier_reactive(&test.identifier, reactive_ids, reactive_decls)
                || cases.iter().any(|case| {
                    case.test.as_ref().is_some_and(|case_test| {
                        is_identifier_reactive(&case_test.identifier, reactive_ids, reactive_decls)
                    })
                })
        }
        _ => false,
    }
}

fn visit_and_prune_terminal(
    terminal: &mut ReactiveTerminal,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
    reassigned_decls: &HashSet<DeclarationId>,
    original_named_ids: &HashMap<DeclarationId, IdentifierId>,
    stable_pruned_function_aliases: &HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            visit_and_prune_block(
                consequent,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
            if let Some(alt) = alternate {
                visit_and_prune_block(
                    alt,
                    reactive_ids,
                    reactive_decls,
                    reassigned_decls,
                    original_named_ids,
                    stable_pruned_function_aliases,
                );
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    visit_and_prune_block(
                        block,
                        reactive_ids,
                        reactive_decls,
                        reassigned_decls,
                        original_named_ids,
                        stable_pruned_function_aliases,
                    );
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            visit_and_prune_block(
                loop_block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            visit_and_prune_block(
                init,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
            if let Some(upd) = update {
                visit_and_prune_block(
                    upd,
                    reactive_ids,
                    reactive_decls,
                    reassigned_decls,
                    original_named_ids,
                    stable_pruned_function_aliases,
                );
            }
            visit_and_prune_block(
                loop_block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            visit_and_prune_block(
                init,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
            visit_and_prune_block(
                loop_block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_and_prune_block(
                init,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
            visit_and_prune_block(
                loop_block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_and_prune_block(
                block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            visit_and_prune_block(
                block,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
            visit_and_prune_block(
                handler,
                reactive_ids,
                reactive_decls,
                reassigned_decls,
                original_named_ids,
                stable_pruned_function_aliases,
            );
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_reassigned_decl_ids(func: &ReactiveFunction) -> HashSet<DeclarationId> {
    fn visit_block(block: &ReactiveBlock, out: &mut HashSet<DeclarationId>) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                        && lvalue.kind == InstructionKind::Reassign
                        && lvalue.place.identifier.name.is_some()
                    {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => visit_terminal(&term_stmt.terminal, out),
                ReactiveStatement::Scope(scope_block) => {
                    visit_block(&scope_block.instructions, out)
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    visit_block(&scope_block.instructions, out)
                }
            }
        }
    }

    fn visit_terminal(terminal: &ReactiveTerminal, out: &mut HashSet<DeclarationId>) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                visit_block(consequent, out);
                if let Some(alt) = alternate {
                    visit_block(alt, out);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        visit_block(block, out);
                    }
                }
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. } => visit_block(loop_block, out),
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                visit_block(init, out);
                if let Some(upd) = update {
                    visit_block(upd, out);
                }
                visit_block(loop_block, out);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            }
            | ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                visit_block(init, out);
                visit_block(loop_block, out);
            }
            ReactiveTerminal::Label { block, .. } => visit_block(block, out),
            ReactiveTerminal::Try { block, handler, .. } => {
                visit_block(block, out);
                visit_block(handler, out);
            }
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }

    let mut out = HashSet::new();
    visit_block(&func.body, &mut out);
    out
}

fn collect_original_named_identifier_ids(
    func: &ReactiveFunction,
) -> HashMap<DeclarationId, IdentifierId> {
    fn visit_block(block: &ReactiveBlock, out: &mut HashMap<DeclarationId, IdentifierId>) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if let Some(lvalue) = &instr.lvalue
                        && lvalue.identifier.name.is_some()
                    {
                        out.entry(lvalue.identifier.declaration_id)
                            .or_insert(lvalue.identifier.id);
                    }
                    match &instr.value {
                        InstructionValue::StoreLocal { lvalue, .. }
                        | InstructionValue::StoreContext { lvalue, .. }
                        | InstructionValue::DeclareLocal { lvalue, .. }
                        | InstructionValue::DeclareContext { lvalue, .. } => {
                            if lvalue.place.identifier.name.is_some() {
                                out.entry(lvalue.place.identifier.declaration_id)
                                    .or_insert(lvalue.place.identifier.id);
                            }
                        }
                        InstructionValue::Destructure { lvalue, .. } => {
                            for_each_pattern_place(&lvalue.pattern, &mut |place| {
                                if place.identifier.name.is_some() {
                                    out.entry(place.identifier.declaration_id)
                                        .or_insert(place.identifier.id);
                                }
                            });
                        }
                        _ => {}
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    visit_terminal(&term_stmt.terminal, out);
                }
                ReactiveStatement::Scope(scope_block) => {
                    visit_block(&scope_block.instructions, out)
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    visit_block(&scope_block.instructions, out)
                }
            }
        }
    }

    fn visit_terminal(terminal: &ReactiveTerminal, out: &mut HashMap<DeclarationId, IdentifierId>) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                visit_block(consequent, out);
                if let Some(alt) = alternate {
                    visit_block(alt, out);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        visit_block(block, out);
                    }
                }
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. } => visit_block(loop_block, out),
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                visit_block(init, out);
                if let Some(update) = update {
                    visit_block(update, out);
                }
                visit_block(loop_block, out);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            }
            | ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                visit_block(init, out);
                visit_block(loop_block, out);
            }
            ReactiveTerminal::Label { block, .. } => visit_block(block, out),
            ReactiveTerminal::Try { block, handler, .. } => {
                visit_block(block, out);
                visit_block(handler, out);
            }
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }

    let mut out = HashMap::new();
    for param in &func.params {
        match param {
            Argument::Place(place) | Argument::Spread(place) => {
                if place.identifier.name.is_some() {
                    out.entry(place.identifier.declaration_id)
                        .or_insert(place.identifier.id);
                }
            }
        }
    }
    visit_block(&func.body, &mut out);
    out
}

fn collect_stable_pruned_function_aliases(func: &ReactiveFunction) -> HashSet<DeclarationId> {
    fn collect_named_in_pruned_scopes(block: &ReactiveBlock, out: &mut HashSet<DeclarationId>) {
        for stmt in block {
            match stmt {
                ReactiveStatement::PrunedScope(scope_block) => {
                    out.extend(collect_named_function_decl_ids(&scope_block.instructions));
                    collect_named_in_pruned_scopes(&scope_block.instructions, out);
                }
                ReactiveStatement::Scope(scope_block) => {
                    collect_named_in_pruned_scopes(&scope_block.instructions, out);
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    collect_named_in_pruned_scopes_from_terminal(&term_stmt.terminal, out);
                }
                ReactiveStatement::Instruction(_) => {}
            }
        }
    }

    fn collect_named_in_pruned_scopes_from_terminal(
        terminal: &ReactiveTerminal,
        out: &mut HashSet<DeclarationId>,
    ) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                collect_named_in_pruned_scopes(consequent, out);
                if let Some(alt) = alternate {
                    collect_named_in_pruned_scopes(alt, out);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        collect_named_in_pruned_scopes(block, out);
                    }
                }
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. } => {
                collect_named_in_pruned_scopes(loop_block, out);
            }
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                collect_named_in_pruned_scopes(init, out);
                if let Some(update) = update {
                    collect_named_in_pruned_scopes(update, out);
                }
                collect_named_in_pruned_scopes(loop_block, out);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            }
            | ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                collect_named_in_pruned_scopes(init, out);
                collect_named_in_pruned_scopes(loop_block, out);
            }
            ReactiveTerminal::Label { block, .. } => {
                collect_named_in_pruned_scopes(block, out);
            }
            ReactiveTerminal::Try { block, handler, .. } => {
                collect_named_in_pruned_scopes(block, out);
                collect_named_in_pruned_scopes(handler, out);
            }
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }

    fn propagate_aliases(block: &ReactiveBlock, stable: &mut HashSet<DeclarationId>) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        if stable.contains(&value.identifier.declaration_id) {
                            stable.insert(lvalue.place.identifier.declaration_id);
                        }
                    }
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        if let Some(lvalue) = &instr.lvalue
                            && stable.contains(&place.identifier.declaration_id)
                        {
                            stable.insert(lvalue.identifier.declaration_id);
                        }
                    }
                    _ => {}
                },
                ReactiveStatement::Terminal(term_stmt) => {
                    propagate_aliases_from_terminal(&term_stmt.terminal, stable);
                }
                ReactiveStatement::Scope(scope_block) => {
                    propagate_aliases(&scope_block.instructions, stable)
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    propagate_aliases(&scope_block.instructions, stable)
                }
            }
        }
    }

    fn propagate_aliases_from_terminal(
        terminal: &ReactiveTerminal,
        stable: &mut HashSet<DeclarationId>,
    ) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                propagate_aliases(consequent, stable);
                if let Some(alt) = alternate {
                    propagate_aliases(alt, stable);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        propagate_aliases(block, stable);
                    }
                }
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. } => propagate_aliases(loop_block, stable),
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                propagate_aliases(init, stable);
                if let Some(update) = update {
                    propagate_aliases(update, stable);
                }
                propagate_aliases(loop_block, stable);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            }
            | ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                propagate_aliases(init, stable);
                propagate_aliases(loop_block, stable);
            }
            ReactiveTerminal::Label { block, .. } => propagate_aliases(block, stable),
            ReactiveTerminal::Try { block, handler, .. } => {
                propagate_aliases(block, stable);
                propagate_aliases(handler, stable);
            }
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }

    let mut stable = HashSet::new();
    collect_named_in_pruned_scopes(&func.body, &mut stable);
    loop {
        let before = stable.len();
        propagate_aliases(&func.body, &mut stable);
        if stable.len() == before {
            break;
        }
    }
    stable
}

/// Propagate reactivity through data-flow instructions.
/// This mirrors the upstream Visitor in PruneNonReactiveDependencies.ts.
fn propagate_reactivity(
    instr: &ReactiveInstruction,
    reactive_ids: &mut HashSet<IdentifierId>,
    reactive_decls: &mut HashSet<DeclarationId>,
) {
    match &instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            // If source is reactive, lvalue is reactive
            if let Some(lvalue) = &instr.lvalue
                && is_identifier_reactive(&place.identifier, reactive_ids, reactive_decls)
            {
                mark_reactive_identifier(
                    "propagate:load_result",
                    &lvalue.identifier,
                    reactive_ids,
                    reactive_decls,
                );
            }
        }
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            // If value is reactive, lvalue target and instruction lvalue are reactive
            if is_identifier_reactive(&value.identifier, reactive_ids, reactive_decls) {
                mark_reactive_identifier(
                    "propagate:store_target",
                    &lvalue.place.identifier,
                    reactive_ids,
                    reactive_decls,
                );
                if let Some(instr_lvalue) = &instr.lvalue {
                    mark_reactive_identifier(
                        "propagate:store_lvalue",
                        &instr_lvalue.identifier,
                        reactive_ids,
                        reactive_decls,
                    );
                }
            }
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            // If destructured value is reactive, all pattern operands are reactive
            // (unless they have a stable type)
            if is_identifier_reactive(&value.identifier, reactive_ids, reactive_decls) {
                for_each_pattern_place(&lvalue.pattern, &mut |place| {
                    if !is_stable_type(&place.identifier) {
                        mark_reactive_identifier(
                            "propagate:destructure_place",
                            &place.identifier,
                            reactive_ids,
                            reactive_decls,
                        );
                    }
                });
                if let Some(instr_lvalue) = &instr.lvalue {
                    mark_reactive_identifier(
                        "propagate:destructure_lvalue",
                        &instr_lvalue.identifier,
                        reactive_ids,
                        reactive_decls,
                    );
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            // If object is reactive and result is not stable, result is reactive
            if let Some(lvalue) = &instr.lvalue
                && is_identifier_reactive(&object.identifier, reactive_ids, reactive_decls)
                && !is_stable_type(&lvalue.identifier)
            {
                mark_reactive_identifier(
                    "propagate:property_load",
                    &lvalue.identifier,
                    reactive_ids,
                    reactive_decls,
                );
            }
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            // If object OR property is reactive, result is reactive
            if let Some(lvalue) = &instr.lvalue
                && (is_identifier_reactive(&object.identifier, reactive_ids, reactive_decls)
                    || is_identifier_reactive(&property.identifier, reactive_ids, reactive_decls))
            {
                mark_reactive_identifier(
                    "propagate:computed_load",
                    &lvalue.identifier,
                    reactive_ids,
                    reactive_decls,
                );
            }
        }
        // Upstream does not propagate reactivity through other instruction kinds here.
        // In particular, expression results such as calls, object/array literals,
        // ternaries, updates, and function expressions do not become reactive solely
        // because one of their operands was reactive.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_identifier(id: u32, name: Option<IdentifierName>) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32, name: Option<IdentifierName>, reactive: bool) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Unknown,
            reactive,
            loc: SourceLocation::Generated,
        }
    }

    fn make_scope_with_deps(id: u32, dep_ids: Vec<u32>) -> ReactiveScope {
        let deps = dep_ids
            .into_iter()
            .map(|dep_id| ReactiveScopeDependency {
                identifier: make_identifier(dep_id, None),
                path: vec![],
            })
            .collect();
        ReactiveScope {
            id: ScopeId(id),
            range: MutableRange::default(),
            dependencies: deps,
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_prune_non_reactive_dep() {
        // Scope depends on id=1, but id=1 is not reactive => dep should be pruned
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: make_scope_with_deps(1, vec![1]),
                instructions: vec![ReactiveStatement::Instruction(Box::new(
                    ReactiveInstruction {
                        id: InstructionId(0),
                        lvalue: Some(make_place(2, None, false)),
                        value: InstructionValue::Primitive {
                            value: PrimitiveValue::Number(42.0),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                    },
                ))],
            })],
        };

        prune_non_reactive_deps_reactive(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            assert!(
                scope_block.scope.dependencies.is_empty(),
                "Non-reactive dependency should be pruned"
            );
        }
    }

    #[test]
    fn test_keep_reactive_dep() {
        // Scope depends on id=1, and id=1 IS reactive (place.reactive=true)
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                // An instruction that makes id=1 reactive
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1, None, true)), // reactive=true
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_scope_with_deps(1, vec![1]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place(2, None, false)),
                            value: InstructionValue::LoadLocal {
                                place: make_place(1, None, true),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
            ],
        };

        prune_non_reactive_deps_reactive(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[1] {
            assert_eq!(
                scope_block.scope.dependencies.len(),
                1,
                "Reactive dependency should be kept"
            );
        }
    }

    #[test]
    fn test_reactivity_propagates_through_load() {
        // id=1 is reactive, LoadLocal reads it into id=2,
        // scope depends on id=2 => should keep the dependency
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1, None, true)),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_scope_with_deps(1, vec![2]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place(2, None, false)),
                            value: InstructionValue::LoadLocal {
                                place: make_place(1, None, true), // reactive source
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
            ],
        };

        prune_non_reactive_deps_reactive(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[1] {
            assert_eq!(
                scope_block.scope.dependencies.len(),
                1,
                "Dependency should be kept because reactivity propagated through LoadLocal"
            );
        }
    }
}
