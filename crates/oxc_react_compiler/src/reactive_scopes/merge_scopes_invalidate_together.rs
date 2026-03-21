//! Port of `MergeReactiveScopesThatInvalidateTogether.ts` from upstream React Compiler.
//!
//! The primary goal of this pass is to reduce memoization overhead:
//! - Use fewer memo slots
//! - Reduce the number of comparisons and other memoization-related instructions
//!
//! The algorithm merges in two main cases:
//!
//! ## Consecutive Scopes
//! If two consecutive scopes in the same block would always invalidate together,
//! it is more efficient to merge them. We merge when:
//! - The scopes have identical dependencies.
//! - The output of scope A is the input to scope B (and the type guarantees invalidation).
//!
//! Intermediate instructions between scopes are only safe to absorb if they are
//! simple (LoadLocal, Primitive, etc.) and their values are not used after the
//! second scope.
//!
//! ## Nested Scopes
//! If an inner scope has the same dependencies as its parent, the inner scope is
//! flattened away since it always invalidates at the same time.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::object_shape::{
    BUILT_IN_ARRAY_ID, BUILT_IN_FUNCTION_ID, BUILT_IN_JSX_ID, BUILT_IN_OBJECT_ID,
};
use crate::hir::types::*;

fn debug_scope_merge_enabled() -> bool {
    std::env::var("DEBUG_SCOPE_MERGE").is_ok() || std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok()
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Merge reactive scopes that invalidate together.
///
/// This is a two-pass algorithm over the `ReactiveFunction` tree:
/// 1. Find the last usage (max InstructionId) of every DeclarationId.
/// 2. Transform: flatten nested scopes with identical deps, and merge
///    consecutive scopes that invalidate together.
pub fn merge_scopes_invalidate_together(func: &mut ReactiveFunction) {
    // Pass 1: find last usage of every declaration
    let mut last_usage: HashMap<DeclarationId, InstructionId> = HashMap::new();
    find_last_usage_block(&func.body, &mut last_usage);

    // Collect declarations that are context-backed in this reactive function.
    // Upstream generally emits `LoadContext` for these reads; our lowering can
    // still produce `LoadLocal`, which should not be treated as merge-safe.
    let mut context_declaration_ids: HashSet<DeclarationId> = HashSet::new();
    collect_context_declaration_ids_block(&func.body, &mut context_declaration_ids);

    // Pass 2: transform
    let mut temporaries: HashMap<DeclarationId, DeclarationId> = HashMap::new();
    transform_block(
        &mut func.body,
        None,
        &last_usage,
        &mut temporaries,
        &context_declaration_ids,
    );
}

// ---------------------------------------------------------------------------
// Pass 1: FindLastUsage
// ---------------------------------------------------------------------------

/// Visit every Place in the reactive function tree, recording the maximum
/// InstructionId at which each DeclarationId is referenced.
fn visit_place_for_last_usage(
    id: InstructionId,
    place: &Place,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    let decl_id = place.identifier.declaration_id;
    let entry = last_usage.entry(decl_id).or_insert(id);
    if id.0 > entry.0 {
        *entry = id;
    }
}

fn find_last_usage_block(
    block: &ReactiveBlock,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                find_last_usage_instruction(instr, last_usage);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                find_last_usage_terminal(&term_stmt.terminal, last_usage);
            }
            ReactiveStatement::Scope(scope_block) => {
                find_last_usage_block(&scope_block.instructions, last_usage);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                find_last_usage_block(&scope_block.instructions, last_usage);
            }
        }
    }
}

fn find_last_usage_instruction(
    instr: &ReactiveInstruction,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    let id = instr.id;
    // Visit lvalue
    if let Some(lvalue) = &instr.lvalue {
        visit_place_for_last_usage(id, lvalue, last_usage);
    }
    // Visit instruction value operands and lvalues
    visit_instruction_value_places(id, &instr.value, last_usage);
}

/// Visit all places (operands and lvalues) within an InstructionValue.
fn visit_instruction_value_places(
    id: InstructionId,
    value: &InstructionValue,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            visit_place_for_last_usage(id, place, last_usage);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            visit_place_for_last_usage(id, &lvalue.place, last_usage);
        }
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            visit_place_for_last_usage(id, &lvalue.place, last_usage);
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            visit_pattern_places(id, &lvalue.pattern, last_usage);
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            visit_place_for_last_usage(id, left, last_usage);
            visit_place_for_last_usage(id, right, last_usage);
        }
        InstructionValue::UnaryExpression { value, .. } => {
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            visit_place_for_last_usage(id, callee, last_usage);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            visit_place_for_last_usage(id, receiver, last_usage);
            visit_place_for_last_usage(id, property, last_usage);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            visit_place_for_last_usage(id, callee, last_usage);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        visit_place_for_last_usage(id, &p.place, last_usage);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            visit_place_for_last_usage(id, place, last_usage);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(p) = tag {
                visit_place_for_last_usage(id, p, last_usage);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => {
                        visit_place_for_last_usage(id, place, last_usage);
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        visit_place_for_last_usage(id, argument, last_usage);
                    }
                }
            }
            if let Some(children) = children {
                for child in children {
                    visit_place_for_last_usage(id, child, last_usage);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                visit_place_for_last_usage(id, child, last_usage);
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            visit_place_for_last_usage(id, object, last_usage);
        }
        InstructionValue::PropertyStore { object, value, .. } => {
            visit_place_for_last_usage(id, object, last_usage);
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            visit_place_for_last_usage(id, object, last_usage);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            visit_place_for_last_usage(id, object, last_usage);
            visit_place_for_last_usage(id, property, last_usage);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            visit_place_for_last_usage(id, object, last_usage);
            visit_place_for_last_usage(id, property, last_usage);
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            visit_place_for_last_usage(id, object, last_usage);
            visit_place_for_last_usage(id, property, last_usage);
        }
        InstructionValue::TypeCastExpression { value, .. } => {
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for sub in subexprs {
                visit_place_for_last_usage(id, sub, last_usage);
            }
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            visit_place_for_last_usage(id, tag, last_usage);
        }
        InstructionValue::Await { value, .. } => {
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::GetIterator { collection, .. } => {
            visit_place_for_last_usage(id, collection, last_usage);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            visit_place_for_last_usage(id, iterator, last_usage);
            visit_place_for_last_usage(id, collection, last_usage);
        }
        InstructionValue::NextPropertyOf { value, .. } => {
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::PrefixUpdate { lvalue, value, .. }
        | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
            visit_place_for_last_usage(id, lvalue, last_usage);
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            visit_place_for_last_usage(id, test, last_usage);
            visit_place_for_last_usage(id, consequent, last_usage);
            visit_place_for_last_usage(id, alternate, last_usage);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            visit_place_for_last_usage(id, left, last_usage);
            visit_place_for_last_usage(id, right, last_usage);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    visit_place_for_last_usage(id, lvalue, last_usage);
                }
                visit_instruction_value_places(id, &instr.value, last_usage);
            }
            visit_instruction_value_places(id, value, last_usage);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            visit_instruction_value_places(id, value, last_usage);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            visit_instruction_value_places(id, left, last_usage);
            visit_instruction_value_places(id, right, last_usage);
        }
        InstructionValue::StoreGlobal { value, .. } => {
            visit_place_for_last_usage(id, value, last_usage);
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            // Match upstream visitors: function/object method operands are context captures.
            for place in &lowered_func.func.context {
                visit_place_for_last_usage(id, place, last_usage);
            }
        }
        InstructionValue::StartMemoize { .. } | InstructionValue::FinishMemoize { .. } => {
            // FinishMemoize has a `decl` Place but we handle it via the lvalue
        }
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

fn visit_pattern_places(
    id: InstructionId,
    pattern: &Pattern,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        visit_place_for_last_usage(id, &p.place, last_usage);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            visit_place_for_last_usage(id, place, last_usage);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        visit_place_for_last_usage(id, p, last_usage);
                    }
                }
            }
        }
    }
}

fn find_last_usage_terminal(
    terminal: &ReactiveTerminal,
    last_usage: &mut HashMap<DeclarationId, InstructionId>,
) {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        ReactiveTerminal::Return { value, id, .. } | ReactiveTerminal::Throw { value, id, .. } => {
            visit_place_for_last_usage(*id, value, last_usage);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            id,
            ..
        } => {
            visit_place_for_last_usage(*id, test, last_usage);
            find_last_usage_block(consequent, last_usage);
            if let Some(alt) = alternate {
                find_last_usage_block(alt, last_usage);
            }
        }
        ReactiveTerminal::Switch {
            test, cases, id, ..
        } => {
            visit_place_for_last_usage(*id, test, last_usage);
            for case in cases {
                if let Some(test) = &case.test {
                    visit_place_for_last_usage(*id, test, last_usage);
                }
                if let Some(block) = &case.block {
                    find_last_usage_block(block, last_usage);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            id,
            ..
        } => {
            visit_place_for_last_usage(*id, test, last_usage);
            find_last_usage_block(init, last_usage);
            if let Some(upd) = update {
                find_last_usage_block(upd, last_usage);
            }
            find_last_usage_block(loop_block, last_usage);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            id,
            ..
        } => {
            visit_place_for_last_usage(*id, test, last_usage);
            find_last_usage_block(init, last_usage);
            find_last_usage_block(loop_block, last_usage);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            find_last_usage_block(init, last_usage);
            find_last_usage_block(loop_block, last_usage);
        }
        ReactiveTerminal::DoWhile {
            loop_block,
            test,
            id,
            ..
        } => {
            find_last_usage_block(loop_block, last_usage);
            visit_place_for_last_usage(*id, test, last_usage);
        }
        ReactiveTerminal::While {
            test,
            loop_block,
            id,
            ..
        } => {
            visit_place_for_last_usage(*id, test, last_usage);
            find_last_usage_block(loop_block, last_usage);
        }
        ReactiveTerminal::Label { block, .. } => {
            find_last_usage_block(block, last_usage);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            id,
            ..
        } => {
            find_last_usage_block(block, last_usage);
            if let Some(binding) = handler_binding {
                visit_place_for_last_usage(*id, binding, last_usage);
            }
            find_last_usage_block(handler, last_usage);
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 2: Transform
// ---------------------------------------------------------------------------

/// Recursively transform a reactive block.
///
/// `parent_deps` is the dependency set of the enclosing scope (if any).
/// When an inner scope has the same deps as its parent, it is flattened.
fn transform_block(
    block: &mut ReactiveBlock,
    parent_deps: Option<&[ReactiveScopeDependency]>,
    last_usage: &HashMap<DeclarationId, InstructionId>,
    temporaries: &mut HashMap<DeclarationId, DeclarationId>,
    context_declaration_ids: &HashSet<DeclarationId>,
) {
    // Sub-pass A: Flatten nested scopes that have identical deps to their parent.
    // We do this first (like the upstream Transform.transformScope which is called
    // before visitBlock).
    flatten_nested_scopes(
        block,
        parent_deps,
        last_usage,
        temporaries,
        context_declaration_ids,
    );

    // Sub-pass B: Merge consecutive scopes in this block.
    merge_consecutive_scopes(block, last_usage, temporaries, context_declaration_ids);
}

/// Flatten nested scopes: if a Scope has the same dependencies as its parent,
/// replace it with its inner instructions (removing the scope wrapper).
fn flatten_nested_scopes(
    block: &mut ReactiveBlock,
    parent_deps: Option<&[ReactiveScopeDependency]>,
    last_usage: &HashMap<DeclarationId, InstructionId>,
    temporaries: &mut HashMap<DeclarationId, DeclarationId>,
    context_declaration_ids: &HashSet<DeclarationId>,
) {
    let debug_scope_merge = debug_scope_merge_enabled();
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Scope(scope_block) => {
                // First, recurse into the scope with ITS dependencies as the parent
                let scope_deps = scope_block.scope.dependencies.clone();
                transform_block(
                    &mut scope_block.instructions,
                    Some(&scope_deps),
                    last_usage,
                    temporaries,
                    context_declaration_ids,
                );

                // Check if this scope should be flattened (same deps as parent)
                if let Some(p_deps) = parent_deps {
                    let equal_deps =
                        are_equal_dependencies(p_deps, &scope_block.scope.dependencies);
                    if debug_scope_merge {
                        let format_dep = |dep: &ReactiveScopeDependency| -> String {
                            let name = dep
                                .identifier
                                .name
                                .as_ref()
                                .map_or("<unnamed>".to_string(), |n| n.value().to_string());
                            let path = dep
                                .path
                                .iter()
                                .map(|entry| {
                                    if entry.optional {
                                        format!("?.{}", entry.property)
                                    } else {
                                        format!(".{}", entry.property)
                                    }
                                })
                                .collect::<String>();
                            format!("{}:{}{}", dep.identifier.declaration_id.0, name, path)
                        };
                        let parent = p_deps.iter().map(format_dep).collect::<Vec<_>>();
                        let child = scope_block
                            .scope
                            .dependencies
                            .iter()
                            .map(format_dep)
                            .collect::<Vec<_>>();
                        eprintln!(
                            "[SCOPE_MERGE] flatten_check scope={} equal={} parent={:?} child={:?}",
                            scope_block.scope.id.0, equal_deps, parent, child
                        );
                    }
                    if !equal_deps {
                        i += 1;
                        continue;
                    }
                    // Flatten: replace this scope with its instructions
                    let stmt = block.remove(i);
                    if let ReactiveStatement::Scope(scope_block) = stmt {
                        let count = scope_block.instructions.len();
                        for (j, inner_stmt) in scope_block.instructions.into_iter().enumerate() {
                            block.insert(i + j, inner_stmt);
                        }
                        // Re-process from the same index (the inserted stmts might
                        // themselves be scopes that need flattening)
                        i += count;
                        continue;
                    }
                }
                i += 1;
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                // Recurse into pruned scope instructions (no merging, just traversal)
                transform_block(
                    &mut scope_block.instructions,
                    None,
                    last_usage,
                    temporaries,
                    context_declaration_ids,
                );
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                transform_terminal(
                    &mut term_stmt.terminal,
                    parent_deps,
                    last_usage,
                    temporaries,
                    context_declaration_ids,
                );
                i += 1;
            }
            ReactiveStatement::Instruction(_) => {
                i += 1;
            }
        }
    }
}

fn transform_terminal(
    terminal: &mut ReactiveTerminal,
    parent_deps: Option<&[ReactiveScopeDependency]>,
    last_usage: &HashMap<DeclarationId, InstructionId>,
    temporaries: &mut HashMap<DeclarationId, DeclarationId>,
    context_declaration_ids: &HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            transform_block(
                consequent,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
            if let Some(alt) = alternate {
                transform_block(
                    alt,
                    parent_deps,
                    last_usage,
                    temporaries,
                    context_declaration_ids,
                );
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    transform_block(
                        block,
                        parent_deps,
                        last_usage,
                        temporaries,
                        context_declaration_ids,
                    );
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            transform_block(
                loop_block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            transform_block(
                init,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
            if let Some(upd) = update {
                transform_block(
                    upd,
                    parent_deps,
                    last_usage,
                    temporaries,
                    context_declaration_ids,
                );
            }
            transform_block(
                loop_block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            transform_block(
                init,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
            transform_block(
                loop_block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            transform_block(
                init,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
            transform_block(
                loop_block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::Label { block, .. } => {
            transform_block(
                block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            transform_block(
                block,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
            transform_block(
                handler,
                parent_deps,
                last_usage,
                temporaries,
                context_declaration_ids,
            );
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Merge consecutive scopes
// ---------------------------------------------------------------------------

/// Tracks a range of block indices that should be merged into a single scope.
struct MergedScope {
    /// Index in the block where the first scope lives.
    from: usize,
    /// Index (exclusive) up to which we have merged.
    to: usize,
}

/// Identify and merge consecutive scopes in a single block.
///
/// This follows the upstream algorithm closely:
/// 1. Walk the block, identifying merge ranges.
///    When merging scope B into scope A (at `from`), immediately update A's
///    range and declarations so that subsequent comparisons against scope C
///    use the merged state (matching upstream behavior).
/// 2. Reconstruct the block with merged scopes.
fn merge_consecutive_scopes(
    block: &mut ReactiveBlock,
    last_usage: &HashMap<DeclarationId, InstructionId>,
    temporaries: &mut HashMap<DeclarationId, DeclarationId>,
    context_declaration_ids: &HashSet<DeclarationId>,
) {
    let debug_scope_merge = std::env::var("DEBUG_SCOPE_MERGE").is_ok()
        || std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok();

    // --- Phase 1: Identify merge ranges ---
    //
    // We need to compare scopes and update the "current" scope in-place as we
    // merge, just like the upstream does with `current.block`. To work around
    // Rust's borrow rules, we extract the scope data we need for comparison
    // from block[i] before mutating block[from].
    let mut current_from: Option<usize> = None;
    let mut current_to: usize = 0;
    let mut current_lvalues: HashSet<DeclarationId> = HashSet::new();
    let mut merged: Vec<MergedScope> = Vec::new();

    let len = block.len();
    for i in 0..len {
        // We need to determine the kind of block[i] without borrowing the full block
        let stmt_kind = classify_statement(&block[i]);

        match stmt_kind {
            StmtKind::Terminal | StmtKind::PrunedScope => {
                // Don't merge across terminals or pruned scopes
                if let Some(from) = current_from.take() {
                    if current_to > from + 1 {
                        merged.push(MergedScope {
                            from,
                            to: current_to,
                        });
                    }
                    current_lvalues.clear();
                }
            }
            StmtKind::Instruction => {
                handle_instruction_for_merge(
                    &block[i],
                    &mut current_from,
                    &mut current_to,
                    &mut current_lvalues,
                    &mut merged,
                    temporaries,
                    context_declaration_ids,
                );
            }
            StmtKind::Scope => {
                if let Some(from) = current_from {
                    if debug_scope_merge
                        && let (
                            ReactiveStatement::Scope(current_scope),
                            ReactiveStatement::Scope(next_scope),
                        ) = (&block[from], &block[i])
                    {
                        let format_dep = |dep: &ReactiveScopeDependency| -> String {
                            let name = dep
                                .identifier
                                .name
                                .as_ref()
                                .map_or("<unnamed>".to_string(), |n| n.value().to_string());
                            format!("{}:{}", dep.identifier.declaration_id.0, name)
                        };
                        let format_decl = |decl: &ScopeDeclaration| -> String {
                            let name = decl
                                .identifier
                                .name
                                .as_ref()
                                .map_or("<unnamed>".to_string(), |n| n.value().to_string());
                            format!("{}:{}", decl.identifier.declaration_id.0, name)
                        };
                        let current_deps = current_scope
                            .scope
                            .dependencies
                            .iter()
                            .map(format_dep)
                            .collect::<Vec<_>>();
                        let next_deps = next_scope
                            .scope
                            .dependencies
                            .iter()
                            .map(format_dep)
                            .collect::<Vec<_>>();
                        let current_decls = current_scope
                            .scope
                            .declarations
                            .values()
                            .map(format_decl)
                            .collect::<Vec<_>>();
                        let next_decls = next_scope
                            .scope
                            .declarations
                            .values()
                            .map(format_decl)
                            .collect::<Vec<_>>();
                        eprintln!(
                            "[SCOPE_MERGE] consider cur_scope={} cur_range=({}, {}) cur_deps={:?} cur_decls={:?} next_scope={} next_range=({}, {}) next_deps={:?} next_decls={:?}",
                            current_scope.scope.id.0,
                            current_scope.scope.range.start.0,
                            current_scope.scope.range.end.0,
                            current_deps,
                            current_decls,
                            next_scope.scope.id.0,
                            next_scope.scope.range.start.0,
                            next_scope.scope.range.end.0,
                            next_deps,
                            next_decls
                        );
                    }
                    // Extract data from the scopes for comparison
                    let next_scope_data = extract_scope_data(&block[i]);
                    let current_scope_data = extract_scope_data(&block[from]);

                    if let (Some(cur), Some(nxt)) = (current_scope_data, next_scope_data) {
                        let can_merge = can_merge_scope_data(&cur, &nxt, temporaries);
                        let lvalues_ok =
                            are_lvalues_last_used_by_scope_data(&nxt, &current_lvalues, last_usage);
                        if debug_scope_merge && (!can_merge || !lvalues_ok) {
                            eprintln!(
                                "[SCOPE_MERGE] reject from={} to={} can_merge={} lvalues_ok={} current_lvalues={:?}",
                                from,
                                i,
                                can_merge,
                                lvalues_ok,
                                current_lvalues.iter().map(|id| id.0).collect::<Vec<_>>(),
                            );
                        }
                        if can_merge && lvalues_ok {
                            if debug_scope_merge {
                                eprintln!(
                                    "[SCOPE_MERGE] merge from={} to={} merged_range_end={}",
                                    from, i, nxt.range_end
                                );
                            }
                            // Merge! Update the scope at `from` in-place.
                            // We use the already-extracted nxt.declarations to
                            // avoid borrowing block[i] while mutating block[from].
                            if let ReactiveStatement::Scope(ref mut target) = block[from] {
                                // Extend range
                                target.scope.range.end =
                                    InstructionId(target.scope.range.end.0.max(nxt.range_end));
                                // Add declarations from the merged scope
                                for (key, value) in &nxt.declarations {
                                    target.scope.declarations.insert(*key, value.clone());
                                }
                                // Prune declarations no longer needed
                                update_scope_declarations(&mut target.scope, last_usage);
                            }
                            current_to = i + 1;
                            current_lvalues.clear();

                            // Check if the merged scope is still eligible for further merging
                            let next_eligible = is_scope_data_eligible_for_merging(&nxt);
                            if !next_eligible {
                                if current_to > from + 1 {
                                    merged.push(MergedScope {
                                        from,
                                        to: current_to,
                                    });
                                }
                                current_from = None;
                                current_lvalues.clear();
                            }
                            continue;
                        }
                    }

                    // Cannot merge. Reset.
                    if current_to > from + 1 {
                        merged.push(MergedScope {
                            from,
                            to: current_to,
                        });
                    }
                    current_from = None;
                    current_lvalues.clear();
                }

                // Check if this scope can be a new merge candidate
                let scope_data = extract_scope_data(&block[i]);
                if let Some(sd) = scope_data
                    && is_scope_data_eligible_for_merging(&sd)
                {
                    current_from = Some(i);
                    current_to = i + 1;
                    current_lvalues.clear();
                }
            }
        }
    }
    // Final reset
    if let Some(from) = current_from.take()
        && current_to > from + 1
    {
        merged.push(MergedScope {
            from,
            to: current_to,
        });
    }

    // --- Phase 2: Apply merges ---
    if merged.is_empty() {
        return;
    }

    let mut next_instructions: Vec<ReactiveStatement> = Vec::with_capacity(block.len());
    let mut index = 0;

    for entry in &merged {
        // Copy everything before the merge range
        while index < entry.from {
            next_instructions.push(block[index].take_placeholder());
            index += 1;
        }

        // The first scope in the range is the "target" scope (already has
        // updated range and declarations from Phase 1).
        let mut target = block[index].take_placeholder();
        index += 1;

        // Absorb subsequent entries into the target scope
        while index < entry.to {
            let item = block[index].take_placeholder();
            index += 1;

            match item {
                ReactiveStatement::Scope(other_scope) => {
                    // Merge the other scope's instructions into target
                    if let ReactiveStatement::Scope(ref mut target_scope) = target {
                        for instr in other_scope.instructions {
                            target_scope.instructions.push(instr);
                        }
                        // Note: upstream does scope.merged.add(other.scope.id)
                        // but merged_id is Option<ScopeId> in our types, so we skip
                        // this tracking for now (it's only informational).
                    }
                }
                other => {
                    // Intermediate instruction -- absorb into the target scope
                    if let ReactiveStatement::Scope(ref mut target_scope) = target {
                        target_scope.instructions.push(other);
                    }
                }
            }
        }

        next_instructions.push(target);
    }

    // Copy remaining
    while index < block.len() {
        next_instructions.push(block[index].take_placeholder());
        index += 1;
    }

    *block = next_instructions;
}

// ---------------------------------------------------------------------------
// Helpers for the merge identification phase
// ---------------------------------------------------------------------------

/// Classification of a ReactiveStatement for merge logic.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StmtKind {
    Instruction,
    Terminal,
    Scope,
    PrunedScope,
}

fn classify_statement(stmt: &ReactiveStatement) -> StmtKind {
    match stmt {
        ReactiveStatement::Instruction(_) => StmtKind::Instruction,
        ReactiveStatement::Terminal(_) => StmtKind::Terminal,
        ReactiveStatement::Scope(_) => StmtKind::Scope,
        ReactiveStatement::PrunedScope(_) => StmtKind::PrunedScope,
    }
}

/// Extracted data from a ReactiveScopeBlock needed for merge comparisons.
/// This avoids holding a borrow on the block array.
#[allow(dead_code)]
struct ScopeData {
    dependencies: Vec<ReactiveScopeDependency>,
    declarations: Vec<(IdentifierId, ScopeDeclaration)>,
    has_reassignments: bool,
    range_start: u32,
    range_end: u32,
}

fn extract_scope_data(stmt: &ReactiveStatement) -> Option<ScopeData> {
    if let ReactiveStatement::Scope(sb) = stmt {
        Some(ScopeData {
            dependencies: sb.scope.dependencies.clone(),
            declarations: sb
                .scope
                .declarations
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            has_reassignments: !sb.scope.reassignments.is_empty(),
            range_start: sb.scope.range.start.0,
            range_end: sb.scope.range.end.0,
        })
    } else {
        None
    }
}

/// Like `can_merge_scopes` but operates on extracted ScopeData.
fn can_merge_scope_data(
    current: &ScopeData,
    next: &ScopeData,
    temporaries: &HashMap<DeclarationId, DeclarationId>,
) -> bool {
    // Don't merge scopes with reassignments
    if current.has_reassignments || next.has_reassignments {
        return false;
    }

    // Merge if dependencies are identical
    if are_equal_dependencies(&current.dependencies, &next.dependencies) {
        return true;
    }

    // Merge if outputs of current are inputs to next
    let decl_as_deps: Vec<ReactiveScopeDependency> = current
        .declarations
        .iter()
        .map(|(_, decl)| ReactiveScopeDependency {
            identifier: decl.identifier.clone(),
            path: vec![],
        })
        .collect();
    if are_equal_dependencies(&decl_as_deps, &next.dependencies) {
        return true;
    }

    // Case 2: all next deps are always-invalidating types and match current declarations.
    if !next.dependencies.is_empty()
        && next.dependencies.iter().all(|dep| {
            let path_ok = dep.path.is_empty();
            let type_ok = is_always_invalidating_type(&dep.identifier.type_);
            // Upstream: check if dependency's declarationId matches any
            // declaration in current scope, or is reachable via temporaries.
            let decl_match = current.declarations.iter().any(|(_, decl)| {
                decl.identifier.declaration_id == dep.identifier.declaration_id
                    || temporaries.get(&dep.identifier.declaration_id)
                        == Some(&decl.identifier.declaration_id)
            });
            path_ok && type_ok && decl_match
        })
    {
        return true;
    }

    false
}

/// Like `are_lvalues_last_used_by_scope` but uses extracted ScopeData.
fn are_lvalues_last_used_by_scope_data(
    scope: &ScopeData,
    lvalues: &HashSet<DeclarationId>,
    last_usage: &HashMap<DeclarationId, InstructionId>,
) -> bool {
    let debug_scope_merge = debug_scope_merge_enabled();
    for lvalue in lvalues {
        if let Some(&last_used_at) = last_usage.get(lvalue) {
            if last_used_at.0 >= scope.range_end {
                if debug_scope_merge {
                    eprintln!(
                        "[SCOPE_MERGE] reject lvalue decl={} last_used={} scope_end={}",
                        lvalue.0, last_used_at.0, scope.range_end
                    );
                }
                return false;
            }
            if debug_scope_merge {
                eprintln!(
                    "[SCOPE_MERGE] keep lvalue decl={} last_used={} scope_end={}",
                    lvalue.0, last_used_at.0, scope.range_end
                );
            }
        }
    }
    true
}

/// Is this scope eligible for merging with subsequent scopes?
///
/// A scope is eligible if:
/// - It has no dependencies (output never changes), OR
/// - At least one of its declarations has an always-invalidating type.
#[cfg(test)]
fn scope_is_eligible_for_merging(scope_block: &ReactiveScopeBlock) -> bool {
    let data = extract_scope_data(&ReactiveStatement::Scope(ReactiveScopeBlock {
        scope: scope_block.scope.clone(),
        instructions: vec![],
    }));
    data.is_some_and(|sd| is_scope_data_eligible_for_merging(&sd))
}

/// Mirrors upstream `scopeIsEligibleForMerging`:
/// - If no dependencies, the output never changes, so eligible.
/// - If any declaration has an always-invalidating type, eligible.
fn is_scope_data_eligible_for_merging(scope: &ScopeData) -> bool {
    if scope.dependencies.is_empty() {
        return true;
    }
    scope
        .declarations
        .iter()
        .any(|(_, decl)| is_always_invalidating_type(&decl.identifier.type_))
}

/// Handle an instruction statement during the merge identification phase.
fn handle_instruction_for_merge(
    stmt: &ReactiveStatement,
    current_from: &mut Option<usize>,
    _current_to: &mut usize,
    current_lvalues: &mut HashSet<DeclarationId>,
    merged: &mut Vec<MergedScope>,
    temporaries: &mut HashMap<DeclarationId, DeclarationId>,
    context_declaration_ids: &HashSet<DeclarationId>,
) {
    let debug_scope_merge = debug_scope_merge_enabled();
    let instr = match stmt {
        ReactiveStatement::Instruction(i) => i,
        _ => return,
    };

    if debug_scope_merge {
        eprintln!(
            "[SCOPE_MERGE] inspect instr#{} kind={} has_lvalue={}",
            instr.id.0,
            instruction_value_kind_name(&instr.value),
            instr
                .lvalue
                .as_ref()
                .map(|lv| lv.identifier.declaration_id.0.to_string())
                .unwrap_or_else(|| "<none>".to_string())
        );
    }

    match &instr.value {
        InstructionValue::LoadLocal { place, .. } => {
            // Upstream treats context-backed reads as `LoadContext`, which is not
            // in the merge-safe instruction subset. Our lowering can leave these
            // as `LoadLocal`; treat them equivalently here.
            if context_declaration_ids.contains(&place.identifier.declaration_id) {
                if let Some(from) = current_from.take()
                    && *_current_to > from + 1
                {
                    merged.push(MergedScope {
                        from,
                        to: *_current_to,
                    });
                }
                current_lvalues.clear();
                if debug_scope_merge {
                    eprintln!(
                        "[SCOPE_MERGE] reset on context-like LoadLocal instr#{} decl={}",
                        instr.id.0, place.identifier.declaration_id.0
                    );
                }
                return;
            }

            // Safe LoadLocal case (non-context).
            if current_from.is_some()
                && let Some(lvalue) = &instr.lvalue
            {
                let decl_id = lvalue.identifier.declaration_id;
                current_lvalues.insert(decl_id);
                temporaries.insert(decl_id, place.identifier.declaration_id);
                if debug_scope_merge {
                    eprintln!(
                        "[SCOPE_MERGE] track safe instr lvalue decl={} from kind={}",
                        decl_id.0,
                        instruction_value_kind_name(&instr.value)
                    );
                }
            }
        }
        InstructionValue::BinaryExpression { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::UnaryExpression { .. } => {
            // Safe instructions: can be absorbed into a merged scope
            if current_from.is_some()
                && let Some(lvalue) = &instr.lvalue
            {
                let decl_id = lvalue.identifier.declaration_id;
                current_lvalues.insert(decl_id);
                if debug_scope_merge {
                    eprintln!(
                        "[SCOPE_MERGE] track safe instr lvalue decl={} from kind={}",
                        decl_id.0,
                        instruction_value_kind_name(&instr.value)
                    );
                }
            }
        }
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            // StoreLocal is safe only if it's a Const assignment
            if current_from.is_some() {
                if lvalue.kind == InstructionKind::Const {
                    // Record all lvalues produced by this instruction
                    current_lvalues.insert(lvalue.place.identifier.declaration_id);
                    if let Some(outer_lvalue) = &instr.lvalue {
                        current_lvalues.insert(outer_lvalue.identifier.declaration_id);
                    }
                    // Track the temporary chain
                    temporaries.insert(
                        lvalue.place.identifier.declaration_id,
                        temporaries
                            .get(&value.identifier.declaration_id)
                            .copied()
                            .unwrap_or(value.identifier.declaration_id),
                    );
                    if debug_scope_merge {
                        eprintln!(
                            "[SCOPE_MERGE] track StoreLocal const lvalue={} source={}",
                            lvalue.place.identifier.declaration_id.0,
                            temporaries
                                .get(&lvalue.place.identifier.declaration_id)
                                .copied()
                                .unwrap_or(value.identifier.declaration_id)
                                .0
                        );
                    }
                } else {
                    // Reassignment -- not safe to merge
                    if let Some(from) = current_from.take() {
                        if *_current_to > from + 1 {
                            merged.push(MergedScope {
                                from,
                                to: *_current_to,
                            });
                        }
                        current_lvalues.clear();
                        if debug_scope_merge {
                            eprintln!(
                                "[SCOPE_MERGE] reset on non-const StoreLocal from={} to={}",
                                from, *_current_to
                            );
                        }
                    }
                }
            }
        }
        _ => {
            // Other instructions prevent merging
            if let Some(from) = current_from.take() {
                if *_current_to > from + 1 {
                    merged.push(MergedScope {
                        from,
                        to: *_current_to,
                    });
                }
                current_lvalues.clear();
                if debug_scope_merge {
                    eprintln!(
                        "[SCOPE_MERGE] reset on blocking instr kind={} from={} to={}",
                        instruction_value_kind_name(&instr.value),
                        from,
                        *_current_to
                    );
                }
            }
        }
    }
}

fn collect_context_declaration_ids_block(
    block: &ReactiveBlock,
    context_declaration_ids: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } = &instr.value
                {
                    context_declaration_ids.insert(lvalue.place.identifier.declaration_id);
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_context_declaration_ids_block(
                    &scope_block.instructions,
                    context_declaration_ids,
                );
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_context_declaration_ids_block(
                    &scope_block.instructions,
                    context_declaration_ids,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_context_declaration_ids_terminal(
                    &term_stmt.terminal,
                    context_declaration_ids,
                );
            }
        }
    }
}

fn collect_context_declaration_ids_terminal(
    terminal: &ReactiveTerminal,
    context_declaration_ids: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_context_declaration_ids_block(consequent, context_declaration_ids);
            if let Some(alt) = alternate {
                collect_context_declaration_ids_block(alt, context_declaration_ids);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_context_declaration_ids_block(block, context_declaration_ids);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_context_declaration_ids_block(loop_block, context_declaration_ids);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_context_declaration_ids_block(init, context_declaration_ids);
            if let Some(upd) = update {
                collect_context_declaration_ids_block(upd, context_declaration_ids);
            }
            collect_context_declaration_ids_block(loop_block, context_declaration_ids);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            collect_context_declaration_ids_block(init, context_declaration_ids);
            collect_context_declaration_ids_block(loop_block, context_declaration_ids);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_context_declaration_ids_block(init, context_declaration_ids);
            collect_context_declaration_ids_block(loop_block, context_declaration_ids);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_context_declaration_ids_block(block, context_declaration_ids);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_context_declaration_ids_block(block, context_declaration_ids);
            collect_context_declaration_ids_block(handler, context_declaration_ids);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn instruction_value_kind_name(value: &InstructionValue) -> &'static str {
    match value {
        InstructionValue::DeclareContext { .. } => "DeclareContext",
        InstructionValue::StoreContext { .. } => "StoreContext",
        InstructionValue::LoadContext { .. } => "LoadContext",
        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
        InstructionValue::StoreLocal { .. } => "StoreLocal",
        InstructionValue::LoadLocal { .. } => "LoadLocal",
        InstructionValue::FunctionExpression { .. } => "FunctionExpression",
        InstructionValue::ObjectMethod { .. } => "ObjectMethod",
        InstructionValue::Primitive { .. } => "Primitive",
        InstructionValue::RegExpLiteral { .. } => "RegExpLiteral",
        InstructionValue::JsxExpression { .. } => "JsxExpression",
        InstructionValue::JsxFragment { .. } => "JsxFragment",
        InstructionValue::JSXText { .. } => "JSXText",
        InstructionValue::TaggedTemplateExpression { .. } => "TaggedTemplateExpression",
        InstructionValue::TemplateLiteral { .. } => "TemplateLiteral",
        InstructionValue::NewExpression { .. } => "NewExpression",
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::TypeCastExpression { .. } => "TypeCastExpression",
        InstructionValue::ObjectExpression { .. } => "ObjectExpression",
        InstructionValue::ArrayExpression { .. } => "ArrayExpression",
        InstructionValue::Await { .. } => "Await",
        InstructionValue::BinaryExpression { .. } => "BinaryExpression",
        InstructionValue::UnaryExpression { .. } => "UnaryExpression",
        InstructionValue::PropertyLoad { .. } => "PropertyLoad",
        InstructionValue::PropertyStore { .. } => "PropertyStore",
        InstructionValue::PropertyDelete { .. } => "PropertyDelete",
        InstructionValue::ComputedLoad { .. } => "ComputedLoad",
        InstructionValue::ComputedStore { .. } => "ComputedStore",
        InstructionValue::ComputedDelete { .. } => "ComputedDelete",
        InstructionValue::StoreGlobal { .. } => "StoreGlobal",
        InstructionValue::LoadGlobal { .. } => "LoadGlobal",
        InstructionValue::Destructure { .. } => "Destructure",
        InstructionValue::Debugger { .. } => "Debugger",
        InstructionValue::Ternary { .. } => "Ternary",
        InstructionValue::LogicalExpression { .. } => "LogicalExpression",
        InstructionValue::PrefixUpdate { .. } => "PrefixUpdate",
        InstructionValue::PostfixUpdate { .. } => "PostfixUpdate",
        InstructionValue::GetIterator { .. } => "GetIterator",
        InstructionValue::IteratorNext { .. } => "IteratorNext",
        InstructionValue::NextPropertyOf { .. } => "NextPropertyOf",
        InstructionValue::StartMemoize { .. } => "StartMemoize",
        InstructionValue::FinishMemoize { .. } => "FinishMemoize",
        InstructionValue::MetaProperty { .. } => "MetaProperty",
        InstructionValue::ReactiveSequenceExpression { .. } => "ReactiveSequenceExpression",
        InstructionValue::ReactiveOptionalExpression { .. } => "ReactiveOptionalExpression",
        InstructionValue::ReactiveLogicalExpression { .. } => "ReactiveLogicalExpression",
    }
}

// ---------------------------------------------------------------------------
// Helper: dependency comparison
// ---------------------------------------------------------------------------

/// Check if two dependency sets are equal (order-independent).
///
/// Mirrors upstream `areEqualDependencies`.
fn are_equal_dependencies(a: &[ReactiveScopeDependency], b: &[ReactiveScopeDependency]) -> bool {
    fn dependency_key(dep: &ReactiveScopeDependency) -> String {
        let mut key = dep.identifier.declaration_id.0.to_string();
        for entry in &dep.path {
            key.push('/');
            if entry.optional {
                key.push('?');
            }
            key.push_str(&entry.property);
        }
        key
    }

    let a_keys: std::collections::HashSet<String> = a.iter().map(dependency_key).collect();
    let b_keys: std::collections::HashSet<String> = b.iter().map(dependency_key).collect();
    a_keys == b_keys
}

// ---------------------------------------------------------------------------
// Helper: scope merging eligibility
// ---------------------------------------------------------------------------

/// Check if the type is guaranteed to produce a new value when its inputs change.
///
/// Mirrors upstream `isAlwaysInvalidatingType`.
pub fn is_always_invalidating_type(ty: &Type) -> bool {
    match ty {
        Type::Object { shape_id: Some(id) } => matches!(
            id.as_str(),
            s if s == BUILT_IN_ARRAY_ID
                || s == BUILT_IN_OBJECT_ID
                || s == BUILT_IN_FUNCTION_ID
                || s == BUILT_IN_JSX_ID
        ),
        Type::Object { shape_id: None } => false,
        Type::Function { .. } => true,
        _ => false,
    }
}

/// Remove declarations from a scope that are no longer used after the scope's
/// updated range (post-merging).
fn update_scope_declarations(
    scope: &mut ReactiveScope,
    last_usage: &HashMap<DeclarationId, InstructionId>,
) {
    scope.declarations.retain(|_id, decl| {
        if let Some(&last_used_at) = last_usage.get(&decl.identifier.declaration_id) {
            // Keep the declaration if it is used at or after the scope ends.
            last_used_at.0 >= scope.range.end.0
        } else {
            // No usage info -- conservatively keep
            true
        }
    });
}

// ---------------------------------------------------------------------------
// Helper: take-placeholder for ReactiveStatement
// ---------------------------------------------------------------------------

/// Extension trait to take a ReactiveStatement out of a vec slot, leaving
/// a lightweight placeholder.
trait TakePlaceholder {
    fn take_placeholder(&mut self) -> Self;
}

impl TakePlaceholder for ReactiveStatement {
    fn take_placeholder(&mut self) -> Self {
        std::mem::replace(
            self,
            ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                id: InstructionId(0),
                lvalue: None,
                value: InstructionValue::Debugger {
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
            })),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_identifier(id: u32) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: None,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_identifier_with_type(id: u32, ty: Type) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: None,
            mutable_range: MutableRange::default(),
            scope: None,
            type_: ty,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32) -> Place {
        Place {
            identifier: make_identifier(id),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_scope(id: u32, range_start: u32, range_end: u32) -> ReactiveScope {
        ReactiveScope {
            id: ScopeId(id),
            range: MutableRange {
                start: InstructionId(range_start),
                end: InstructionId(range_end),
            },
            dependencies: vec![],
            declarations: Default::default(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    fn make_dep(decl_id: u32) -> ReactiveScopeDependency {
        ReactiveScopeDependency {
            identifier: make_identifier(decl_id),
            path: vec![],
        }
    }

    fn make_func(body: ReactiveBlock) -> ReactiveFunction {
        ReactiveFunction {
            id: None,
            name_hint: None,
            params: vec![],
            body,
        }
    }

    fn make_primitive_instr(id: u32) -> ReactiveStatement {
        ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
            id: InstructionId(id),
            lvalue: Some(make_place(id + 100)),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Number(42.0),
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
        }))
    }

    fn make_load_local_instr(id: u32, src_decl: u32, dst_decl: u32) -> ReactiveStatement {
        ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
            id: InstructionId(id),
            lvalue: Some(Place {
                identifier: make_identifier(dst_decl),
                effect: Effect::Unknown,
                reactive: false,
                loc: SourceLocation::Generated,
            }),
            value: InstructionValue::LoadLocal {
                place: Place {
                    identifier: make_identifier(src_decl),
                    effect: Effect::Unknown,
                    reactive: false,
                    loc: SourceLocation::Generated,
                },
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
        }))
    }

    // -----------------------------------------------------------------------
    // are_equal_dependencies
    // -----------------------------------------------------------------------

    #[test]
    fn test_equal_empty_dependencies() {
        assert!(are_equal_dependencies(&[], &[]));
    }

    #[test]
    fn test_equal_dependencies_same() {
        let a = vec![make_dep(1), make_dep(2)];
        let b = vec![make_dep(1), make_dep(2)];
        assert!(are_equal_dependencies(&a, &b));
    }

    #[test]
    fn test_equal_dependencies_different_order() {
        let a = vec![make_dep(1), make_dep(2)];
        let b = vec![make_dep(2), make_dep(1)];
        assert!(are_equal_dependencies(&a, &b));
    }

    #[test]
    fn test_unequal_dependencies_different_size() {
        let a = vec![make_dep(1)];
        let b = vec![make_dep(1), make_dep(2)];
        assert!(!are_equal_dependencies(&a, &b));
    }

    #[test]
    fn test_unequal_dependencies_different_ids() {
        let a = vec![make_dep(1)];
        let b = vec![make_dep(2)];
        assert!(!are_equal_dependencies(&a, &b));
    }

    // -----------------------------------------------------------------------
    // is_always_invalidating_type
    // -----------------------------------------------------------------------

    #[test]
    fn test_always_invalidating_function_type() {
        let ty = Type::Function {
            shape_id: None,
            return_type: Box::new(Type::Poly),
            is_constructor: false,
        };
        assert!(is_always_invalidating_type(&ty));
    }

    #[test]
    fn test_always_invalidating_object_array() {
        let ty = Type::Object {
            shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
        };
        assert!(is_always_invalidating_type(&ty));
    }

    #[test]
    fn test_not_invalidating_primitive() {
        assert!(!is_always_invalidating_type(&Type::Primitive));
    }

    #[test]
    fn test_not_invalidating_poly() {
        assert!(!is_always_invalidating_type(&Type::Poly));
    }

    // -----------------------------------------------------------------------
    // scope_is_eligible_for_merging
    // -----------------------------------------------------------------------

    #[test]
    fn test_scope_eligible_no_deps() {
        let scope_block = ReactiveScopeBlock {
            scope: make_scope(1, 0, 10),
            instructions: vec![],
        };
        assert!(scope_is_eligible_for_merging(&scope_block));
    }

    #[test]
    fn test_scope_eligible_with_array_decl() {
        let mut scope = make_scope(1, 0, 10);
        scope.dependencies = vec![make_dep(99)];
        scope.declarations.insert(
            IdentifierId(1),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    1,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );
        let scope_block = ReactiveScopeBlock {
            scope,
            instructions: vec![],
        };
        assert!(scope_is_eligible_for_merging(&scope_block));
    }

    #[test]
    fn test_scope_not_eligible_only_primitive_decl() {
        let mut scope = make_scope(1, 0, 10);
        scope.dependencies = vec![make_dep(99)];
        scope.declarations.insert(
            IdentifierId(1),
            ScopeDeclaration {
                identifier: make_identifier_with_type(1, Type::Primitive),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );
        let scope_block = ReactiveScopeBlock {
            scope,
            instructions: vec![],
        };
        assert!(!scope_is_eligible_for_merging(&scope_block));
    }

    // -----------------------------------------------------------------------
    // Merge consecutive scopes with identical deps
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_consecutive_same_deps() {
        let dep = make_dep(99);
        let mut scope1 = make_scope(1, 0, 5);
        scope1.dependencies = vec![dep.clone()];
        scope1.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    10,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut scope2 = make_scope(2, 6, 10);
        scope2.dependencies = vec![make_dep(99)];
        scope2.declarations.insert(
            IdentifierId(20),
            ScopeDeclaration {
                identifier: make_identifier(20),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut func = make_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope1,
                instructions: vec![make_primitive_instr(1)],
            }),
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope2,
                instructions: vec![make_primitive_instr(7)],
            }),
        ]);

        merge_scopes_invalidate_together(&mut func);

        // Should have merged into a single scope
        assert_eq!(func.body.len(), 1);
        assert!(matches!(&func.body[0], ReactiveStatement::Scope(_)));

        if let ReactiveStatement::Scope(merged) = &func.body[0] {
            // The merged scope should contain both instructions
            assert_eq!(merged.instructions.len(), 2);
            // Range should be extended
            assert_eq!(merged.scope.range.end.0, 10);
        }
    }

    // -----------------------------------------------------------------------
    // Don't merge scopes with different deps
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_merge_different_deps() {
        let mut scope1 = make_scope(1, 0, 5);
        scope1.dependencies = vec![make_dep(1)];
        scope1.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    10,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut scope2 = make_scope(2, 6, 10);
        scope2.dependencies = vec![make_dep(2)];

        let mut func = make_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope1,
                instructions: vec![make_primitive_instr(1)],
            }),
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope2,
                instructions: vec![make_primitive_instr(7)],
            }),
        ]);

        merge_scopes_invalidate_together(&mut func);

        // Should remain as two separate scopes
        assert_eq!(func.body.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Flatten nested scope with same deps as parent
    // -----------------------------------------------------------------------

    #[test]
    fn test_flatten_nested_scope_same_deps() {
        let dep = make_dep(99);
        let mut outer_scope = make_scope(1, 0, 20);
        outer_scope.dependencies = vec![dep.clone()];
        outer_scope.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier(10),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut inner_scope = make_scope(2, 5, 15);
        inner_scope.dependencies = vec![make_dep(99)];

        let mut func = make_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: outer_scope,
            instructions: vec![
                make_primitive_instr(1),
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: inner_scope,
                    instructions: vec![make_primitive_instr(8)],
                }),
                make_primitive_instr(16),
            ],
        })]);

        merge_scopes_invalidate_together(&mut func);

        // The outer scope should remain, but the inner scope should be flattened
        assert_eq!(func.body.len(), 1);
        if let ReactiveStatement::Scope(outer) = &func.body[0] {
            // The inner scope's instruction should now be directly in the outer scope
            assert_eq!(outer.instructions.len(), 3);
            // None of the instructions should be a Scope (inner was flattened)
            for instr in &outer.instructions {
                assert!(
                    !matches!(instr, ReactiveStatement::Scope(_)),
                    "Inner scope should have been flattened"
                );
            }
        } else {
            panic!("Expected outer scope to remain");
        }
    }

    // -----------------------------------------------------------------------
    // Don't flatten nested scope with different deps
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_flatten_nested_scope_different_deps() {
        let mut outer_scope = make_scope(1, 0, 20);
        outer_scope.dependencies = vec![make_dep(99)];

        let mut inner_scope = make_scope(2, 5, 15);
        inner_scope.dependencies = vec![make_dep(100)]; // different dep

        let mut func = make_func(vec![ReactiveStatement::Scope(ReactiveScopeBlock {
            scope: outer_scope,
            instructions: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: inner_scope,
                instructions: vec![make_primitive_instr(8)],
            })],
        })]);

        merge_scopes_invalidate_together(&mut func);

        // The inner scope should NOT be flattened
        if let ReactiveStatement::Scope(outer) = &func.body[0] {
            assert_eq!(outer.instructions.len(), 1);
            assert!(matches!(
                &outer.instructions[0],
                ReactiveStatement::Scope(_)
            ));
        }
    }

    // -----------------------------------------------------------------------
    // Don't merge across terminals
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_merge_across_terminal() {
        let dep = make_dep(99);

        let mut scope1 = make_scope(1, 0, 5);
        scope1.dependencies = vec![dep.clone()];
        scope1.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    10,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut scope2 = make_scope(2, 20, 25);
        scope2.dependencies = vec![make_dep(99)];

        let mut func = make_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope1,
                instructions: vec![make_primitive_instr(1)],
            }),
            // Terminal between scopes prevents merging
            ReactiveStatement::Terminal(ReactiveTerminalStatement {
                terminal: ReactiveTerminal::If {
                    test: make_place(50),
                    consequent: vec![],
                    alternate: None,
                    id: InstructionId(10),
                },
                label: None,
            }),
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope2,
                instructions: vec![make_primitive_instr(21)],
            }),
        ]);

        merge_scopes_invalidate_together(&mut func);

        // Should remain as three separate items (two scopes + terminal)
        assert_eq!(func.body.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Merge with intermediate safe instructions
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_with_intermediate_load_local() {
        let dep = make_dep(99);

        let mut scope1 = make_scope(1, 0, 5);
        scope1.dependencies = vec![dep.clone()];
        scope1.declarations.insert(
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    10,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut scope2 = make_scope(2, 8, 12);
        scope2.dependencies = vec![make_dep(99)];
        scope2.declarations.insert(
            IdentifierId(20),
            ScopeDeclaration {
                identifier: make_identifier(20),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        // Intermediate LoadLocal instruction whose lvalue is only used in scope2
        let intermediate = make_load_local_instr(6, 50, 51);

        let mut func = make_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope1,
                instructions: vec![make_primitive_instr(1)],
            }),
            intermediate,
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope2,
                instructions: vec![make_primitive_instr(9)],
            }),
        ]);

        merge_scopes_invalidate_together(&mut func);

        // Should merge: 1 scope with 3 items (original instr + intermediate + other instr)
        assert_eq!(func.body.len(), 1);
        if let ReactiveStatement::Scope(merged) = &func.body[0] {
            assert_eq!(merged.instructions.len(), 3);
        }
    }

    // -----------------------------------------------------------------------
    // Don't merge scopes with reassignments
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_merge_with_reassignments() {
        let dep = make_dep(99);

        let mut scope1 = make_scope(1, 0, 5);
        scope1.dependencies = vec![dep.clone()];
        scope1.reassignments = vec![make_identifier(50)]; // has reassignment

        let mut scope2 = make_scope(2, 6, 10);
        scope2.dependencies = vec![make_dep(99)];

        let mut func = make_func(vec![
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope1,
                instructions: vec![make_primitive_instr(1)],
            }),
            ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: scope2,
                instructions: vec![make_primitive_instr(7)],
            }),
        ]);

        merge_scopes_invalidate_together(&mut func);

        // Should NOT merge because scope1 has reassignments
        assert_eq!(func.body.len(), 2);
    }

    #[test]
    fn test_can_merge_scope_data_with_temporary_dependency_mapping() {
        let mut current = ScopeData {
            dependencies: vec![make_dep(1)],
            declarations: Vec::new(),
            has_reassignments: false,
            range_start: 10,
            range_end: 20,
        };
        current.declarations.push((
            IdentifierId(10),
            ScopeDeclaration {
                identifier: make_identifier_with_type(
                    10,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                scope: make_declaration_scope(ScopeId(1)),
            },
        ));

        let next = ScopeData {
            dependencies: vec![ReactiveScopeDependency {
                identifier: make_identifier_with_type(
                    20,
                    Type::Object {
                        shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                    },
                ),
                path: vec![],
            }],
            declarations: Default::default(),
            has_reassignments: false,
            range_start: 21,
            range_end: 30,
        };

        let temporaries = HashMap::from([(DeclarationId(20), DeclarationId(10))]);
        assert!(can_merge_scope_data(&current, &next, &temporaries));
    }
}
