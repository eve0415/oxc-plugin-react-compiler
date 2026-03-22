//! Prune unused lvalue assignments from reactive instructions.
//!
//! Port of `PruneTemporaryLValues.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Nulls out lvalues for temporary variables that are never accessed later.
//!
//! Port adaptation: flattened value blocks can also leave behind dead alias
//! instructions (`LoadLocal`, `LoadContext`, `TypeCastExpression`) whose only
//! purpose was to forward a temporary result. Once their lvalue is pruned,
//! they should be removed entirely instead of degrading into standalone
//! expression statements during codegen.
use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

/// Removes unused lvalue assignments from reactive instructions.
///
/// Walks the reactive tree in source order using upstream semantics:
/// 1. When a place is visited later, it clears any pending temporary lvalue
///    candidate for that declaration.
/// 2. After visiting an instruction, if its lvalue is an unnamed temporary,
///    it becomes the new pending candidate for that declaration.
/// 3. Any candidate still pending after traversal is unused and gets pruned.
///
/// Uses DeclarationIds because the lvalue IdentifierId of a compound
/// expression (ternary, logical, optional) in ReactiveFunction may not be the
/// same as the IdentifierId of the phi which is referenced later.
///
/// Port adaptation: flattened value-block lowering can create multiple dead
/// temp writes to the same declaration before the real use appears. When a
/// new temp write overwrites an older pending one, prune the older write too.
pub fn prune_unused_lvalues(func: &mut ReactiveFunction) {
    while prune_unused_lvalues_once(func) {}
}

fn prune_unused_lvalues_once(func: &mut ReactiveFunction) -> bool {
    let mut lvalue_candidates: HashMap<DeclarationId, InstructionId> = HashMap::new();
    let mut to_prune: HashSet<InstructionId> = HashSet::new();
    collect_lvalues_and_refs(&func.body, &mut lvalue_candidates, &mut to_prune);
    to_prune.extend(lvalue_candidates.values().copied());

    if to_prune.is_empty() {
        return false;
    }

    // Pre-compute the set of DeclarationIds that have HoistedLet declarations
    // anywhere in the reactive function. This is needed by
    // should_preserve_reassign_result_load to distinguish TDZ-check reads
    // (which must be kept) from dead comma-expression artifact reads (which
    // should be dropped).
    let mut hoisted_let_decls = HashSet::new();
    collect_hoisted_let_decl_ids(&func.body, &mut hoisted_let_decls);

    prune_lvalues_in_block(&mut func.body, &to_prune, &hoisted_let_decls)
}

fn collect_lvalues_and_refs(
    block: &ReactiveBlock,
    candidates: &mut HashMap<DeclarationId, InstructionId>,
    to_prune: &mut HashSet<InstructionId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                collect_refs_from_value(&instr.value, candidates);
                if let Some(lvalue) = &instr.lvalue
                    && lvalue.identifier.name.is_none()
                    && let Some(prev) =
                        candidates.insert(lvalue.identifier.declaration_id, instr.id)
                {
                    to_prune.insert(prev);
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_refs_from_terminal(&term_stmt.terminal, candidates, to_prune);
            }
            ReactiveStatement::Scope(scope) => {
                // Visit scope metadata references (matching upstream's visitor
                // which calls visitPlace for all places including scope
                // declarations, dependencies, and reassignments).
                for decl in scope.scope.declarations.values() {
                    candidates.remove(&decl.identifier.declaration_id);
                }
                for dep in &scope.scope.dependencies {
                    candidates.remove(&dep.identifier.declaration_id);
                }
                for reassignment in &scope.scope.reassignments {
                    candidates.remove(&reassignment.declaration_id);
                }
                if let Some(early_return) = &scope.scope.early_return_value {
                    candidates.remove(&early_return.value.declaration_id);
                }
                collect_lvalues_and_refs(&scope.instructions, candidates, to_prune);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_lvalues_and_refs(&scope.instructions, candidates, to_prune);
            }
        }
    }
}

fn collect_refs_from_place(place: &Place, candidates: &mut HashMap<DeclarationId, InstructionId>) {
    candidates.remove(&place.identifier.declaration_id);
}

fn collect_refs_from_value(
    value: &InstructionValue,
    candidates: &mut HashMap<DeclarationId, InstructionId>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            collect_refs_from_place(place, candidates);
        }
        InstructionValue::StoreLocal {
            lvalue, value: val, ..
        }
        | InstructionValue::StoreContext {
            lvalue, value: val, ..
        } => {
            collect_refs_from_place(val, candidates);
            collect_refs_from_place(&lvalue.place, candidates);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            collect_refs_from_place(&lvalue.place, candidates);
        }
        InstructionValue::Destructure {
            value: val,
            lvalue: pat,
            ..
        } => {
            collect_refs_from_place(val, candidates);
            collect_refs_from_pattern(pat, candidates);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            collect_refs_from_place(left, candidates);
            collect_refs_from_place(right, candidates);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            collect_refs_from_place(callee, candidates);
            for arg in args {
                collect_refs_from_arg(arg, candidates);
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            collect_refs_from_place(receiver, candidates);
            collect_refs_from_place(property, candidates);
            for arg in args {
                collect_refs_from_arg(arg, candidates);
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            collect_refs_from_place(callee, candidates);
            for arg in args {
                collect_refs_from_arg(arg, candidates);
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            collect_refs_from_place(place, candidates);
                        }
                        collect_refs_from_place(&p.place, candidates);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        collect_refs_from_place(place, candidates);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        collect_refs_from_place(place, candidates);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            collect_refs_from_place(object, candidates);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            collect_refs_from_place(object, candidates);
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            collect_refs_from_place(object, candidates);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            collect_refs_from_place(object, candidates);
            collect_refs_from_place(property, candidates);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            collect_refs_from_place(object, candidates);
            collect_refs_from_place(property, candidates);
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            collect_refs_from_place(object, candidates);
            collect_refs_from_place(property, candidates);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(place) = tag {
                collect_refs_from_place(place, candidates);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => {
                        collect_refs_from_place(place, candidates);
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        collect_refs_from_place(argument, candidates);
                    }
                }
            }
            if let Some(children) = children {
                for child in children {
                    collect_refs_from_place(child, candidates);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                collect_refs_from_place(child, candidates);
            }
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            collect_refs_from_place(test, candidates);
            collect_refs_from_place(consequent, candidates);
            collect_refs_from_place(alternate, candidates);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            collect_refs_from_place(left, candidates);
            collect_refs_from_place(right, candidates);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    collect_refs_from_place(lvalue, candidates);
                }
                collect_refs_from_value(&instr.value, candidates);
            }
            collect_refs_from_value(value, candidates);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            collect_refs_from_value(value, candidates);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            collect_refs_from_value(left, candidates);
            collect_refs_from_value(right, candidates);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            collect_refs_from_place(tag, candidates);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                collect_refs_from_place(expr, candidates);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::GetIterator { collection, .. } => {
            collect_refs_from_place(collection, candidates);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            collect_refs_from_place(iterator, candidates);
            collect_refs_from_place(collection, candidates);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::PrefixUpdate {
            lvalue, value: val, ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue, value: val, ..
        } => {
            collect_refs_from_place(lvalue, candidates);
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            collect_refs_from_place(decl, candidates);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            collect_refs_from_place(val, candidates);
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            // Nested functions close over outer bindings through `context`.
            // Those captures must keep the outer declaration/result lvalues alive;
            // otherwise later codegen can lose the binding and fall back to null.
            for captured in &lowered_func.func.context {
                collect_refs_from_place(captured, candidates);
            }
        }
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn collect_refs_from_arg(arg: &Argument, candidates: &mut HashMap<DeclarationId, InstructionId>) {
    match arg {
        Argument::Place(place) | Argument::Spread(place) => {
            collect_refs_from_place(place, candidates);
        }
    }
}

fn collect_refs_from_pattern(
    pat: &LValuePattern,
    candidates: &mut HashMap<DeclarationId, InstructionId>,
) {
    match &pat.pattern {
        Pattern::Array(arr) => {
            for elem in &arr.items {
                match elem {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        collect_refs_from_place(place, candidates);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        collect_refs_from_place(&p.place, candidates);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        collect_refs_from_place(place, candidates);
                    }
                }
            }
        }
    }
}

fn collect_refs_from_terminal(
    terminal: &ReactiveTerminal,
    candidates: &mut HashMap<DeclarationId, InstructionId>,
    to_prune: &mut HashSet<InstructionId>,
) {
    fn mark_last_temp_lvalue_as_referenced(
        block: &ReactiveBlock,
        candidates: &mut HashMap<DeclarationId, InstructionId>,
    ) {
        let Some(instr) = block.iter().rev().find_map(|stmt| match stmt {
            ReactiveStatement::Instruction(instr) => Some(instr),
            _ => None,
        }) else {
            return;
        };
        let Some(lvalue) = &instr.lvalue else {
            return;
        };
        if lvalue.identifier.name.is_none() {
            candidates.remove(&lvalue.identifier.declaration_id);
        }
    }

    match terminal {
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            collect_refs_from_place(value, candidates);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            collect_refs_from_place(test, candidates);
            collect_lvalues_and_refs(consequent, candidates, to_prune);
            if let Some(alt) = alternate {
                collect_lvalues_and_refs(alt, candidates, to_prune);
            }
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            collect_refs_from_place(test, candidates);
            for case in cases {
                if let Some(t) = &case.test {
                    collect_refs_from_place(t, candidates);
                }
                if let Some(block) = &case.block {
                    collect_lvalues_and_refs(block, candidates, to_prune);
                }
            }
        }
        ReactiveTerminal::DoWhile {
            loop_block, test, ..
        } => {
            collect_lvalues_and_refs(loop_block, candidates, to_prune);
            collect_refs_from_place(test, candidates);
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            collect_refs_from_place(test, candidates);
            collect_lvalues_and_refs(loop_block, candidates, to_prune);
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            collect_lvalues_and_refs(init, candidates, to_prune);
            collect_refs_from_place(test, candidates);
            if let Some(upd) = update {
                collect_lvalues_and_refs(upd, candidates, to_prune);
                // For-update expressions use the trailing value of the update block
                // (e.g. `x = x + y, x`) as the loop header expression result.
                // Preserve the final temp lvalue so codegen can reconstruct this form.
                mark_last_temp_lvalue_as_referenced(upd, candidates);
            }
            collect_lvalues_and_refs(loop_block, candidates, to_prune);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            collect_lvalues_and_refs(init, candidates, to_prune);
            collect_refs_from_place(test, candidates);
            collect_lvalues_and_refs(loop_block, candidates, to_prune);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_lvalues_and_refs(init, candidates, to_prune);
            collect_lvalues_and_refs(loop_block, candidates, to_prune);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_lvalues_and_refs(block, candidates, to_prune);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            collect_lvalues_and_refs(block, candidates, to_prune);
            if let Some(binding) = handler_binding {
                collect_refs_from_place(binding, candidates);
            }
            collect_lvalues_and_refs(handler, candidates, to_prune);
        }
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
    }
}

/// Collect all DeclarationIds that have a HoistedLet declaration anywhere in
/// the reactive function tree.  Used to distinguish TDZ-enforcement reads
/// (must keep) from dead comma-expression artifact reads (should drop).
fn collect_hoisted_let_decl_ids(block: &ReactiveBlock, out: &mut HashSet<DeclarationId>) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if matches!(
                    &instr.value,
                    InstructionValue::DeclareLocal { lvalue, .. }
                        | InstructionValue::DeclareContext { lvalue, .. }
                        if lvalue.kind == InstructionKind::HoistedLet
                ) {
                    if let Some(lv) = &instr.lvalue {
                        out.insert(lv.identifier.declaration_id);
                    }
                    // Also capture the inner lvalue's declaration_id from the
                    // DeclareLocal/DeclareContext value itself.
                    match &instr.value {
                        InstructionValue::DeclareLocal { lvalue, .. }
                        | InstructionValue::DeclareContext { lvalue, .. } => {
                            out.insert(lvalue.place.identifier.declaration_id);
                        }
                        _ => {}
                    }
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_hoisted_let_decl_ids_in_terminal(&term_stmt.terminal, out);
            }
            ReactiveStatement::Scope(scope) => {
                collect_hoisted_let_decl_ids(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_hoisted_let_decl_ids(&scope.instructions, out);
            }
        }
    }
}

fn collect_hoisted_let_decl_ids_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_hoisted_let_decl_ids(consequent, out);
            if let Some(alt) = alternate {
                collect_hoisted_let_decl_ids(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_hoisted_let_decl_ids(block, out);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_hoisted_let_decl_ids(loop_block, out);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_hoisted_let_decl_ids(init, out);
            if let Some(upd) = update {
                collect_hoisted_let_decl_ids(upd, out);
            }
            collect_hoisted_let_decl_ids(loop_block, out);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_hoisted_let_decl_ids(init, out);
            collect_hoisted_let_decl_ids(loop_block, out);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_hoisted_let_decl_ids(block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_hoisted_let_decl_ids(block, out);
            collect_hoisted_let_decl_ids(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn prune_lvalues_in_block(
    block: &mut ReactiveBlock,
    to_prune: &std::collections::HashSet<InstructionId>,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    let mut changed = false;
    for stmt in block.iter_mut() {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if to_prune.contains(&instr.id) && instr.lvalue.take().is_some() {
                    changed = true;
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                changed |=
                    prune_lvalues_in_terminal(&mut term_stmt.terminal, to_prune, hoisted_let_decls);
            }
            ReactiveStatement::Scope(scope) => {
                changed |=
                    prune_lvalues_in_block(&mut scope.instructions, to_prune, hoisted_let_decls);
            }
            ReactiveStatement::PrunedScope(scope) => {
                changed |=
                    prune_lvalues_in_block(&mut scope.instructions, to_prune, hoisted_let_decls);
            }
        }
    }

    changed |= remove_pruned_alias_instructions_in_block(block, to_prune, hoisted_let_decls);

    changed
}

fn prune_lvalues_in_terminal(
    terminal: &mut ReactiveTerminal,
    to_prune: &std::collections::HashSet<InstructionId>,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            let mut changed = prune_lvalues_in_block(consequent, to_prune, hoisted_let_decls);
            if let Some(alt) = alternate {
                changed |= prune_lvalues_in_block(alt, to_prune, hoisted_let_decls);
            }
            changed
        }
        ReactiveTerminal::Switch { cases, .. } => {
            let mut changed = false;
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    changed |= prune_lvalues_in_block(block, to_prune, hoisted_let_decls);
                }
            }
            changed
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            prune_lvalues_in_block(loop_block, to_prune, hoisted_let_decls)
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            let mut changed = prune_lvalues_in_block(init, to_prune, hoisted_let_decls);
            if let Some(upd) = update {
                changed |= prune_lvalues_in_block(upd, to_prune, hoisted_let_decls);
            }
            changed |= prune_lvalues_in_block(loop_block, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            let mut changed = prune_lvalues_in_block(init, to_prune, hoisted_let_decls);
            changed |= prune_lvalues_in_block(loop_block, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            let mut changed = prune_lvalues_in_block(init, to_prune, hoisted_let_decls);
            changed |= prune_lvalues_in_block(loop_block, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::Label { block, .. } => {
            prune_lvalues_in_block(block, to_prune, hoisted_let_decls)
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            let mut changed = prune_lvalues_in_block(block, to_prune, hoisted_let_decls);
            changed |= prune_lvalues_in_block(handler, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => false,
    }
}

fn remove_pruned_alias_instructions_in_block(
    block: &mut ReactiveBlock,
    to_prune: &std::collections::HashSet<InstructionId>,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    let mut changed = false;
    let original = std::mem::take(block);
    let should_drop: Vec<bool> = original
        .iter()
        .enumerate()
        .map(|(index, stmt)| {
            should_drop_pruned_instruction(&original, stmt, index, to_prune, hoisted_let_decls)
        })
        .collect();
    let mut next_block = Vec::with_capacity(original.len());

    for (mut stmt, should_drop) in original.into_iter().zip(should_drop.into_iter()) {
        match &mut stmt {
            ReactiveStatement::Instruction(_instr) => {
                if should_drop {
                    changed = true;
                } else {
                    next_block.push(stmt);
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                changed |= remove_pruned_alias_instructions_in_terminal(
                    &mut term_stmt.terminal,
                    to_prune,
                    hoisted_let_decls,
                );
                next_block.push(stmt);
            }
            ReactiveStatement::Scope(scope) => {
                changed |= remove_pruned_alias_instructions_in_block(
                    &mut scope.instructions,
                    to_prune,
                    hoisted_let_decls,
                );
                next_block.push(stmt);
            }
            ReactiveStatement::PrunedScope(scope) => {
                changed |= remove_pruned_alias_instructions_in_block(
                    &mut scope.instructions,
                    to_prune,
                    hoisted_let_decls,
                );
                next_block.push(stmt);
            }
        }
    }

    *block = next_block;
    changed
}

fn remove_pruned_alias_instructions_in_terminal(
    terminal: &mut ReactiveTerminal,
    to_prune: &std::collections::HashSet<InstructionId>,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            let mut changed =
                remove_pruned_alias_instructions_in_block(consequent, to_prune, hoisted_let_decls);
            if let Some(alternate) = alternate {
                changed |= remove_pruned_alias_instructions_in_block(
                    alternate,
                    to_prune,
                    hoisted_let_decls,
                );
            }
            changed
        }
        ReactiveTerminal::Switch { cases, .. } => {
            let mut changed = false;
            for case in cases {
                if let Some(block) = &mut case.block {
                    changed |= remove_pruned_alias_instructions_in_block(
                        block,
                        to_prune,
                        hoisted_let_decls,
                    );
                }
            }
            changed
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            remove_pruned_alias_instructions_in_block(loop_block, to_prune, hoisted_let_decls)
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            let mut changed =
                remove_pruned_alias_instructions_in_block(init, to_prune, hoisted_let_decls);
            if let Some(update) = update {
                changed |=
                    remove_pruned_alias_instructions_in_block(update, to_prune, hoisted_let_decls);
            }
            changed |=
                remove_pruned_alias_instructions_in_block(loop_block, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            let mut changed =
                remove_pruned_alias_instructions_in_block(init, to_prune, hoisted_let_decls);
            changed |=
                remove_pruned_alias_instructions_in_block(loop_block, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::Label { block, .. } => {
            remove_pruned_alias_instructions_in_block(block, to_prune, hoisted_let_decls)
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            let mut changed =
                remove_pruned_alias_instructions_in_block(block, to_prune, hoisted_let_decls);
            changed |=
                remove_pruned_alias_instructions_in_block(handler, to_prune, hoisted_let_decls);
            changed
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => false,
    }
}

fn should_drop_pruned_instruction(
    stmts: &[ReactiveStatement],
    stmt: &ReactiveStatement,
    index: usize,
    to_prune: &std::collections::HashSet<InstructionId>,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    let ReactiveStatement::Instruction(instr) = stmt else {
        return false;
    };
    if !to_prune.contains(&instr.id) || instr.lvalue.is_some() {
        return false;
    }

    match &instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            let preserve_reassign_result_load = should_preserve_reassign_result_load(
                stmts,
                index,
                place.identifier.declaration_id,
                hoisted_let_decls,
            );
            let preserve_named_scope_bridge = place.identifier.name.is_some()
                && (should_preserve_named_scope_bridge_load(stmts, index)
                    || has_prior_hoisted_let_declaration(
                        &stmts[..index],
                        place.identifier.declaration_id,
                    ));
            !preserve_reassign_result_load
                && !preserve_named_scope_bridge
                && !should_preserve_manual_memo_leading_root_load(
                    stmts,
                    index,
                    place.identifier.declaration_id,
                )
                && !should_preserve_manual_memo_tail_root_load(
                    stmts,
                    index,
                    place.identifier.declaration_id,
                )
        }
        InstructionValue::TypeCastExpression { .. } => true,
        _ => false,
    }
}

/// Preserve a LoadLocal/LoadContext that immediately follows a Reassign
/// StoreLocal/StoreContext for the same variable, but ONLY when the variable
/// has a HoistedLet declaration.  These reads serve as TDZ checks that the
/// upstream emits.  Without the hoisted-let guard, this also incorrectly
/// preserves dead reads from flattened comma expressions in deps lists
/// (e.g., `((y = x.concat(arr2)), y)` produces a trailing `y` that should
/// be dropped when the deps array is removed).
fn should_preserve_reassign_result_load(
    stmts: &[ReactiveStatement],
    index: usize,
    declaration_id: DeclarationId,
    hoisted_let_decls: &HashSet<DeclarationId>,
) -> bool {
    // Only preserve for variables with HoistedLet declarations — these need
    // TDZ enforcement reads after reassignment.
    if !hoisted_let_decls.contains(&declaration_id) {
        return false;
    }

    let Some(ReactiveStatement::Instruction(prev_instr)) = index
        .checked_sub(1)
        .and_then(|prev_index| stmts.get(prev_index))
    else {
        return false;
    };

    matches!(
        &prev_instr.value,
        InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. }
            if lvalue.kind == InstructionKind::Reassign
                && lvalue.place.identifier.declaration_id == declaration_id
    )
}

fn should_preserve_named_scope_bridge_load(stmts: &[ReactiveStatement], index: usize) -> bool {
    matches!(
        stmts.get(index + 1),
        Some(ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_))
    )
}

fn should_preserve_manual_memo_leading_root_load(
    stmts: &[ReactiveStatement],
    index: usize,
    root_decl: DeclarationId,
) -> bool {
    let Some(ReactiveStatement::Instruction(prev_instr)) = index
        .checked_sub(1)
        .and_then(|prev_index| stmts.get(prev_index))
    else {
        return false;
    };

    match &prev_instr.value {
        InstructionValue::StartMemoize { deps, .. } => deps.iter().flatten().any(|dep| {
            dep.path.is_empty()
                && matches!(
                    &dep.root,
                    ManualMemoRoot::NamedLocal(place)
                        if place.identifier.declaration_id == root_decl
                )
        }),
        _ => false,
    }
}

fn has_prior_hoisted_let_declaration(
    stmts: &[ReactiveStatement],
    declaration_id: DeclarationId,
) -> bool {
    for stmt in stmts.iter().rev() {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                    if lvalue.place.identifier.declaration_id == declaration_id
                        && lvalue.kind == InstructionKind::HoistedLet =>
                {
                    return true;
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope) => {
                if has_prior_hoisted_let_declaration(&scope.instructions, declaration_id) {
                    return true;
                }
            }
            ReactiveStatement::PrunedScope(scope) => {
                if has_prior_hoisted_let_declaration(&scope.instructions, declaration_id) {
                    return true;
                }
            }
            ReactiveStatement::Terminal(_) => {}
        }
    }
    false
}

fn should_preserve_manual_memo_tail_root_load(
    stmts: &[ReactiveStatement],
    index: usize,
    root_decl: DeclarationId,
) -> bool {
    let Some(ReactiveStatement::Instruction(next_decl_instr)) = stmts.get(index + 1) else {
        return false;
    };
    let Some(ReactiveStatement::Instruction(finish_instr)) = stmts.get(index + 2) else {
        return false;
    };

    let loaded_decl = match &next_decl_instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            place.identifier.declaration_id
        }
        _ => return false,
    };
    let (manual_memo_id, finish_decl) = match &finish_instr.value {
        InstructionValue::FinishMemoize {
            manual_memo_id,
            decl,
            ..
        } if decl.identifier.declaration_id == loaded_decl
            && matches!(decl.identifier.type_, Type::Function { .. }) =>
        {
            (*manual_memo_id, decl.identifier.declaration_id)
        }
        _ => return false,
    };

    for prev in stmts[..index].iter().rev() {
        let ReactiveStatement::Instruction(prev_instr) = prev else {
            continue;
        };
        match &prev_instr.value {
            InstructionValue::FinishMemoize {
                manual_memo_id: prev_id,
                decl,
                ..
            } if *prev_id == manual_memo_id && decl.identifier.declaration_id == finish_decl => {
                return false;
            }
            InstructionValue::StartMemoize {
                manual_memo_id: prev_id,
                deps,
                ..
            } if *prev_id == manual_memo_id => {
                return deps.iter().flatten().any(|dep| {
                    matches!(
                        &dep.root,
                        ManualMemoRoot::NamedLocal(place)
                            if place.identifier.declaration_id == root_decl
                    )
                });
            }
            _ => {}
        }
    }

    false
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

    fn make_place(id: u32, name: Option<IdentifierName>) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_function_place(id: u32, name: Option<IdentifierName>) -> Place {
        let mut place = make_place(id, name);
        place.identifier.type_ = Type::Function {
            shape_id: None,
            return_type: Box::new(Type::Poly),
            is_constructor: false,
        };
        place
    }

    #[test]
    fn test_prune_unreferenced_temporary_lvalue() {
        // Instruction with unnamed lvalue that is never read => should be pruned
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![ReactiveStatement::Instruction(Box::new(
                ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1, None)), // unnamed temporary
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(42.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
        };

        prune_unused_lvalues(&mut func);

        if let ReactiveStatement::Instruction(instr) = &func.body[0] {
            assert!(
                instr.lvalue.is_none(),
                "Unreferenced temporary lvalue should be pruned"
            );
        }
    }

    #[test]
    fn test_keep_referenced_temporary_lvalue() {
        // Instruction with unnamed lvalue that IS read later => should be kept
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1, None)), // unnamed temporary
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(42.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: None,
                    value: InstructionValue::LoadLocal {
                        place: make_place(1, None), // references declaration 1
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
            ],
        };

        prune_unused_lvalues(&mut func);

        if let ReactiveStatement::Instruction(instr) = &func.body[0] {
            assert!(
                instr.lvalue.is_some(),
                "Referenced temporary lvalue should be kept"
            );
        }
    }

    #[test]
    fn test_keep_named_lvalue() {
        // Instruction with a named lvalue should never be pruned regardless
        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![ReactiveStatement::Instruction(Box::new(
                ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_place(1, Some(IdentifierName::Named("x".to_string())))),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(42.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                },
            ))],
        };

        prune_unused_lvalues(&mut func);

        if let ReactiveStatement::Instruction(instr) = &func.body[0] {
            assert!(
                instr.lvalue.is_some(),
                "Named lvalue should never be pruned"
            );
        }
    }

    #[test]
    fn test_drop_dead_alias_instruction_after_pruning_lvalue() {
        let source = make_place(1, None);
        let alias = make_place(2, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(source.clone()),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(42.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(alias),
                    value: InstructionValue::LoadLocal {
                        place: source.clone(),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: source,
                        id: InstructionId(2),
                    },
                    label: None,
                }),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert_eq!(
            func.body.len(),
            2,
            "dead alias instruction should be removed"
        );
        match &func.body[0] {
            ReactiveStatement::Instruction(instr) => {
                assert_eq!(instr.id, InstructionId(0));
            }
            other => panic!("expected source instruction, got {:?}", other),
        }
        match &func.body[1] {
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::Return { value, .. },
                ..
            }) => {
                assert_eq!(value.identifier.declaration_id, DeclarationId(1));
            }
            other => panic!("expected return terminal, got {:?}", other),
        }
    }

    #[test]
    fn test_drop_dead_alias_chain_to_fixed_point() {
        let source = make_place(1, Some(IdentifierName::Named("y".into())));
        let alias1 = make_place(2, None);
        let alias2 = make_place(3, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(alias1.clone()),
                    value: InstructionValue::LoadLocal {
                        place: source,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(alias2),
                    value: InstructionValue::LoadLocal {
                        place: alias1,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert!(
            func.body.is_empty(),
            "dead alias chain should be removed to a fixed point"
        );
    }

    #[test]
    fn test_preserve_manual_memo_root_load_as_expression() {
        let source = make_place(1, Some(IdentifierName::Named("x".into())));
        let temp = make_place(2, None);
        let memo_decl = make_function_place(3, None);
        let result = make_place(4, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: None,
                    value: InstructionValue::StartMemoize {
                        manual_memo_id: 0,
                        deps: Some(vec![ManualMemoDependency {
                            root: ManualMemoRoot::NamedLocal(source.clone()),
                            path: vec![],
                        }]),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(temp),
                    value: InstructionValue::LoadLocal {
                        place: source,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(2),
                    lvalue: Some(result.clone()),
                    value: InstructionValue::LoadLocal {
                        place: memo_decl.clone(),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(3),
                    lvalue: None,
                    value: InstructionValue::FinishMemoize {
                        manual_memo_id: 0,
                        decl: memo_decl,
                        pruned: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: result,
                        id: InstructionId(4),
                    },
                    label: None,
                }),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert_eq!(func.body.len(), 5);
        match &func.body[1] {
            ReactiveStatement::Instruction(instr) => {
                assert_eq!(instr.id, InstructionId(1));
                assert!(
                    instr.lvalue.is_none(),
                    "manual memo root load should become bare expr"
                );
            }
            other => panic!("expected preserved load instruction, got {:?}", other),
        }
    }

    #[test]
    fn test_preserve_manual_memo_property_root_load_as_expression() {
        let source = make_place(1, Some(IdentifierName::Named("contextVar".into())));
        let temp = make_place(2, None);
        let memo_decl = make_function_place(3, None);
        let result = make_place(4, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: None,
                    value: InstructionValue::StartMemoize {
                        manual_memo_id: 0,
                        deps: Some(vec![ManualMemoDependency {
                            root: ManualMemoRoot::NamedLocal(source.clone()),
                            path: vec![DependencyPathEntry {
                                property: "val".into(),
                                optional: false,
                            }],
                        }]),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(temp),
                    value: InstructionValue::LoadLocal {
                        place: source,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(2),
                    lvalue: Some(result.clone()),
                    value: InstructionValue::LoadLocal {
                        place: memo_decl.clone(),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(3),
                    lvalue: None,
                    value: InstructionValue::FinishMemoize {
                        manual_memo_id: 0,
                        decl: memo_decl,
                        pruned: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: result,
                        id: InstructionId(4),
                    },
                    label: None,
                }),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert_eq!(func.body.len(), 5);
        match &func.body[1] {
            ReactiveStatement::Instruction(instr) => {
                assert_eq!(instr.id, InstructionId(1));
                assert!(
                    instr.lvalue.is_none(),
                    "manual memo property root load should become bare expr"
                );
            }
            other => panic!("expected preserved load instruction, got {:?}", other),
        }
    }

    #[test]
    fn test_preserve_manual_memo_leading_root_load_for_non_function_result() {
        let source = make_place(1, Some(IdentifierName::Named("input".into())));
        let temp = make_place(2, None);
        let memo_decl = make_place(3, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: None,
                    value: InstructionValue::StartMemoize {
                        manual_memo_id: 0,
                        deps: Some(vec![ManualMemoDependency {
                            root: ManualMemoRoot::NamedLocal(source.clone()),
                            path: vec![],
                        }]),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(temp),
                    value: InstructionValue::LoadLocal {
                        place: source,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(2),
                    lvalue: Some(memo_decl.clone()),
                    value: InstructionValue::Primitive {
                        value: PrimitiveValue::Number(1.0),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(3),
                    lvalue: None,
                    value: InstructionValue::FinishMemoize {
                        manual_memo_id: 0,
                        decl: memo_decl.clone(),
                        pruned: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: memo_decl,
                        id: InstructionId(4),
                    },
                    label: None,
                }),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert_eq!(func.body.len(), 5);
        match &func.body[1] {
            ReactiveStatement::Instruction(instr) => {
                assert_eq!(instr.id, InstructionId(1));
                assert!(
                    instr.lvalue.is_none(),
                    "leading manual memo root load should become bare expr"
                );
            }
            other => panic!(
                "expected preserved leading load instruction, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_preserve_manual_memo_root_load_after_alias_drop() {
        let source = make_place(1, Some(IdentifierName::Named("y".into())));
        let temp = make_place(2, None);
        let alias = make_place(3, None);
        let memo_decl = make_function_place(4, None);
        let result = make_place(5, None);

        let mut func = ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body: vec![
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: None,
                    value: InstructionValue::StartMemoize {
                        manual_memo_id: 0,
                        deps: Some(vec![ManualMemoDependency {
                            root: ManualMemoRoot::NamedLocal(source.clone()),
                            path: vec![],
                        }]),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(1),
                    lvalue: Some(temp.clone()),
                    value: InstructionValue::LoadContext {
                        place: source,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(2),
                    lvalue: Some(alias),
                    value: InstructionValue::LoadLocal {
                        place: temp.clone(),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(3),
                    lvalue: Some(result.clone()),
                    value: InstructionValue::LoadLocal {
                        place: memo_decl.clone(),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(4),
                    lvalue: None,
                    value: InstructionValue::FinishMemoize {
                        manual_memo_id: 0,
                        decl: memo_decl,
                        pruned: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: result,
                        id: InstructionId(5),
                    },
                    label: None,
                }),
            ],
        };

        prune_unused_lvalues(&mut func);

        assert_eq!(
            func.body.len(),
            5,
            "dead alias should be dropped without removing the manual memo root load"
        );
        match &func.body[1] {
            ReactiveStatement::Instruction(instr) => {
                assert_eq!(instr.id, InstructionId(1));
                assert!(
                    instr.lvalue.is_none(),
                    "manual memo root load should be preserved as a bare expr after alias removal"
                );
            }
            other => panic!(
                "expected preserved root load after dropped alias, got {:?}",
                other
            ),
        }
        match &func.body[2] {
            ReactiveStatement::Instruction(instr) => assert_eq!(instr.id, InstructionId(3)),
            other => panic!("expected memo decl load, got {:?}", other),
        }
    }
}
