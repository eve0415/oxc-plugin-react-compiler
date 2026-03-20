//! Inline immediately invoked function expressions (IIFEs).
//!
//! Port of `InlineImmediatelyInvokedFunctionExpressions.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use crate::hir::builder::{mark_predecessors, reverse_postorder_blocks};
use crate::hir::merge_consecutive_blocks::merge_consecutive_blocks;
use crate::hir::prune_maybe_throws::mark_instruction_ids;
use crate::hir::types::*;
use crate::hir::visitors::{
    for_each_instruction_value_operand, map_instruction_lvalues, map_instruction_operands,
    map_terminal_operands,
};

/// Inline immediately invoked function expressions to expose finer-grained memoization.
pub fn inline_iifes(func: &mut HIRFunction) {
    let debug = std::env::var_os("DEBUG_INLINE_IIFES").is_some();
    let param_bindings = collect_parameter_bindings(func);
    let mut global_bindings = param_bindings.clone();
    for (_, block) in &func.body.blocks {
        collect_local_bindings_before_call(block, block.instructions.len(), &mut global_bindings);
    }

    // Track all function expressions assigned to temporaries.
    let mut functions: HashMap<IdentifierId, LoweredFunction> = HashMap::new();
    // Track lambdas that were inlined so we can remove their original definitions.
    let mut inlined_functions: HashSet<IdentifierId> = HashSet::new();

    /*
     * Iterate only the outer function's existing blocks. As we inline, we mutate
     * `func` and may append continuation blocks that also need to be visited.
     */
    let mut queue: Vec<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();
    let mut queue_index = 0usize;

    'queue: while queue_index < queue.len() {
        let block_id = queue[queue_index];
        queue_index += 1;

        let Some(block_index) = find_block_index(&func.body.blocks, block_id) else {
            continue;
        };

        if !is_statement_block_kind(func.body.blocks[block_index].1.kind) {
            continue;
        }

        let mut ii = 0usize;
        while ii < func.body.blocks[block_index].1.instructions.len() {
            let instr = func.body.blocks[block_index].1.instructions[ii].clone();

            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    if instr.lvalue.identifier.name.is_none() {
                        if debug {
                            eprintln!(
                                "[INLINE_IIFES] fnexpr temp id={} in block={}",
                                instr.lvalue.identifier.id.0, block_id.0
                            );
                        }
                        functions.insert(instr.lvalue.identifier.id, lowered_func.clone());
                    }
                }
                InstructionValue::CallExpression { callee, args, .. } => {
                    if !args.is_empty() {
                        if debug {
                            eprintln!(
                                "[INLINE_IIFES] skip call id={} args={} block={}",
                                callee.identifier.id.0,
                                args.len(),
                                block_id.0
                            );
                        }
                        ii += 1;
                        continue;
                    }

                    let Some(lowered_func) = functions.get(&callee.identifier.id).cloned() else {
                        if debug {
                            eprintln!(
                                "[INLINE_IIFES] no fn candidate for callee id={} block={}",
                                callee.identifier.id.0, block_id.0
                            );
                        }
                        ii += 1;
                        continue;
                    };

                    if !lowered_func.func.params.is_empty()
                        || lowered_func.func.async_
                        || lowered_func.func.generator
                    {
                        if debug {
                            eprintln!(
                                "[INLINE_IIFES] reject callee id={} params={} async={} generator={} block={}",
                                callee.identifier.id.0,
                                lowered_func.func.params.len(),
                                lowered_func.func.async_,
                                lowered_func.func.generator,
                                block_id.0
                            );
                        }
                        ii += 1;
                        continue;
                    }
                    let has_return_terminal = lowered_func
                        .func
                        .body
                        .blocks
                        .iter()
                        .any(|(_, block)| matches!(block.terminal, Terminal::Return { .. }));
                    if !has_return_terminal {
                        if debug {
                            eprintln!(
                                "[INLINE_IIFES] reject callee id={} no-return-terminal block={}",
                                callee.identifier.id.0, block_id.0
                            );
                        }
                        ii += 1;
                        continue;
                    }

                    // This function expression is being used as an IIFE.
                    inlined_functions.insert(callee.identifier.id);
                    if debug {
                        eprintln!(
                            "[INLINE_IIFES] inline callee id={} block={} instr_idx={}",
                            callee.identifier.id.0, block_id.0, ii
                        );
                    }

                    let continuation_block_id = BlockId(func.env.next_block_id());
                    let mut inlined_body_func = lowered_func.func;
                    let single_exit_return = has_single_exit_return_terminal(&inlined_body_func);

                    let mut result_place = instr.lvalue.clone();
                    let mut local_bindings = global_bindings.clone();
                    collect_local_bindings_before_call(
                        &func.body.blocks[block_index].1,
                        ii,
                        &mut local_bindings,
                    );
                    let captured_store_context_decls = rebind_captured_identifiers(
                        &mut inlined_body_func,
                        &local_bindings,
                        &instr.loc,
                    );
                    if !captured_store_context_decls.is_empty() {
                        upgrade_local_defs_to_context(func, &captured_store_context_decls);
                    }

                    // Split the current block around the IIFE call.
                    let continuation_block = {
                        let (_, block) = &mut func.body.blocks[block_index];

                        let continuation_instructions = if ii < block.instructions.len() {
                            block.instructions.split_off(ii + 1)
                        } else {
                            Vec::new()
                        };

                        // Remove the call itself; keep only instructions before it.
                        block.instructions.truncate(ii);

                        let continuation_terminal = block.terminal.clone();
                        let continuation_terminal_id = continuation_terminal.id();
                        let continuation_terminal_loc = terminal_loc(&continuation_terminal);

                        if single_exit_return {
                            block.terminal = Terminal::Goto {
                                block: inlined_body_func.body.entry,
                                variant: GotoVariant::Break,
                                id: continuation_terminal_id,
                                loc: continuation_terminal_loc.clone(),
                            };
                        } else {
                            block.terminal = Terminal::Label {
                                block: inlined_body_func.body.entry,
                                fallthrough: continuation_block_id,
                                id: make_instruction_id(0),
                                loc: continuation_terminal_loc.clone(),
                            };

                            // Multi-return IIFE needs a persistent result binding.
                            // Upstream mutates the same identifier object before/while
                            // declaring it. In Rust the declaration clones `result_place`,
                            // so we must promote first to keep declaration and subsequent
                            // stores in sync.
                            if result_place.identifier.name.is_none() {
                                promote_temporary(&mut result_place.identifier);
                            }
                            declare_temporary(&func.env, block, &result_place);
                        }

                        BasicBlock {
                            id: continuation_block_id,
                            kind: block.kind,
                            instructions: continuation_instructions,
                            terminal: continuation_terminal,
                            preds: HashSet::new(),
                            phis: Vec::new(),
                        }
                    };

                    if single_exit_return {
                        for (_, nested_block) in &mut inlined_body_func.body.blocks {
                            if let Terminal::Return { value, id, loc, .. } = &nested_block.terminal
                            {
                                let return_value = value.clone();
                                let return_id = *id;
                                let return_loc = loc.clone();

                                nested_block.instructions.push(Instruction {
                                    id: make_instruction_id(0),
                                    loc: return_loc.clone(),
                                    lvalue: result_place.clone(),
                                    value: InstructionValue::LoadLocal {
                                        place: return_value,
                                        loc: return_loc.clone(),
                                    },
                                    effects: None,
                                });

                                nested_block.terminal = Terminal::Goto {
                                    block: continuation_block_id,
                                    variant: GotoVariant::Break,
                                    id: return_id,
                                    loc: return_loc,
                                };
                            }
                        }
                    } else {
                        for (_, nested_block) in &mut inlined_body_func.body.blocks {
                            rewrite_block(
                                &func.env,
                                nested_block,
                                continuation_block_id,
                                &result_place,
                            );
                        }
                    }

                    // Insert continuation and inlined CFG blocks.
                    insert_or_replace_block(&mut func.body, continuation_block);

                    for (_id, mut nested_block) in inlined_body_func.body.blocks {
                        nested_block.preds.clear();
                        insert_or_replace_block(&mut func.body, nested_block);
                    }

                    // Continuation block may contain subsequent IIFEs.
                    queue.push(continuation_block_id);
                    continue 'queue;
                }
                InstructionValue::Ternary { .. } | InstructionValue::LogicalExpression { .. } => {
                    // Upstream lowers these to CFG terminals before this pass.
                    // Rust HIR can keep them as flat instructions, which would
                    // otherwise let us inline an IIFE across a control-flow
                    // boundary that upstream never sees as inlineable.
                    if debug && !functions.is_empty() {
                        eprintln!(
                            "[INLINE_IIFES] clear fn candidates on flat conditional barrier in block={}",
                            block_id.0
                        );
                    }
                    functions.clear();
                }
                _ => {
                    // Any other use of a function temp means it is not an IIFE.
                    for_each_instruction_value_operand(&instr.value, |place| {
                        if debug && functions.contains_key(&place.identifier.id) {
                            eprintln!(
                                "[INLINE_IIFES] invalidate fn candidate id={} via operand use in block={}",
                                place.identifier.id.0, block_id.0
                            );
                        }
                        functions.remove(&place.identifier.id);
                    });
                }
            }

            ii += 1;
        }
    }

    if inlined_functions.is_empty() {
        if debug {
            eprintln!("[INLINE_IIFES] no inlining in function");
        }
        return;
    }

    if debug {
        eprintln!(
            "[INLINE_IIFES] inlined {} function expression(s)",
            inlined_functions.len()
        );
    }

    // Drop original FunctionExpression definitions that were inlined.
    for (_, block) in &mut func.body.blocks {
        block
            .instructions
            .retain(|instr| !inlined_functions.contains(&instr.lvalue.identifier.id));
    }

    // Terminals changed; rebuild a normalized CFG ordering and IDs.
    reverse_postorder_blocks(&mut func.body);
    mark_instruction_ids(&mut func.body);
    mark_predecessors(&mut func.body);
    merge_consecutive_blocks(func);
}

fn find_block_index(blocks: &[(BlockId, BasicBlock)], block_id: BlockId) -> Option<usize> {
    blocks.iter().position(|(id, _)| *id == block_id)
}

fn insert_or_replace_block(body: &mut HIR, block: BasicBlock) {
    if let Some(index) = find_block_index(&body.blocks, block.id) {
        body.blocks[index] = (block.id, block);
    } else {
        body.blocks.push((block.id, block));
    }
}

fn is_statement_block_kind(kind: BlockKind) -> bool {
    matches!(kind, BlockKind::Block | BlockKind::Catch)
}

/// Returns true if the function has exactly one exit terminal and it is a return.
fn has_single_exit_return_terminal(func: &HIRFunction) -> bool {
    let mut has_return = false;
    let mut exit_count = 0usize;

    for (_, block) in &func.body.blocks {
        if matches!(
            block.terminal,
            Terminal::Return { .. } | Terminal::Throw { .. }
        ) {
            if matches!(block.terminal, Terminal::Return { .. }) {
                has_return = true;
            }
            exit_count += 1;
        }
    }

    exit_count == 1 && has_return
}

fn collect_parameter_bindings(func: &HIRFunction) -> HashMap<String, Identifier> {
    let mut bindings = HashMap::new();
    for param in &func.params {
        let place = match param {
            Argument::Place(place) | Argument::Spread(place) => place,
        };
        if let Some(name) = identifier_name_str(&place.identifier) {
            bindings.insert(name.to_string(), place.identifier.clone());
        }
    }
    bindings
}

fn collect_local_bindings_before_call(
    block: &BasicBlock,
    call_index: usize,
    bindings: &mut HashMap<String, Identifier>,
) {
    for instr in block.instructions.iter().take(call_index) {
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::DeclareLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. }
            | InstructionValue::DeclareContext { lvalue, .. } => {
                if let Some(name) = identifier_name_str(&lvalue.place.identifier) {
                    bindings.insert(name.to_string(), lvalue.place.identifier.clone());
                }
            }
            InstructionValue::Destructure { lvalue, .. } => {
                collect_pattern_bindings(&lvalue.pattern, bindings);
            }
            _ => {}
        }
    }
}

fn rebind_captured_identifiers(
    func: &mut HIRFunction,
    local_bindings: &HashMap<String, Identifier>,
    call_site_loc: &SourceLocation,
) -> HashSet<DeclarationId> {
    let locally_declared_names = collect_locally_declared_names(func);
    let visible_before = Some(source_loc_position(call_site_loc));
    let mut captured_store_context_decls: HashSet<DeclarationId> = HashSet::new();

    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            let replacement = match &instr.value {
                InstructionValue::LoadGlobal { binding, loc } => {
                    let name = match binding {
                        NonLocalBinding::Global { name }
                        | NonLocalBinding::ModuleLocal { name } => Some(name.as_str()),
                        _ => None,
                    };
                    name.and_then(|name| {
                        lookup_local_binding_by_capture_name(local_bindings, name, visible_before)
                            .map(|local| InstructionValue::LoadLocal {
                                place: Place {
                                    identifier: local.clone(),
                                    effect: Effect::Unknown,
                                    reactive: false,
                                    loc: loc.clone(),
                                },
                                loc: loc.clone(),
                            })
                    })
                }
                InstructionValue::StoreGlobal { name, value, loc } => {
                    lookup_local_binding_by_capture_name(
                        local_bindings,
                        name.as_str(),
                        visible_before,
                    )
                    .map(|local| {
                        captured_store_context_decls.insert(local.declaration_id);
                        InstructionValue::StoreContext {
                            lvalue: LValue {
                                place: Place {
                                    identifier: local.clone(),
                                    effect: Effect::Unknown,
                                    reactive: false,
                                    loc: loc.clone(),
                                },
                                kind: InstructionKind::Reassign,
                            },
                            value: value.clone(),
                            loc: loc.clone(),
                        }
                    })
                }
                _ => None,
            };
            if let Some(value) = replacement {
                instr.value = value;
            }

            map_instruction_operands(instr, |place| {
                maybe_rebind_captured_local(
                    place,
                    local_bindings,
                    &locally_declared_names,
                    visible_before,
                );
            });
            map_instruction_lvalues(instr, |place| {
                maybe_rebind_captured_local(
                    place,
                    local_bindings,
                    &locally_declared_names,
                    visible_before,
                );
            });
        }

        map_terminal_operands(&mut block.terminal, |place| {
            maybe_rebind_captured_local(
                place,
                local_bindings,
                &locally_declared_names,
                visible_before,
            );
        });
    }

    captured_store_context_decls
}

fn upgrade_local_defs_to_context(func: &mut HIRFunction, decls: &HashSet<DeclarationId>) {
    let undefined_value_ids = collect_undefined_value_ids(func);

    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                    if decls.contains(&lvalue.place.identifier.declaration_id)
                        && !undefined_value_ids.contains(&value.identifier.id) =>
                {
                    if let InstructionValue::StoreLocal { lvalue, value, loc } = std::mem::replace(
                        &mut instr.value,
                        InstructionValue::Debugger {
                            loc: SourceLocation::Generated,
                        },
                    ) {
                        instr.value = InstructionValue::StoreContext { lvalue, value, loc };
                    }
                }
                _ => {}
            }
        }
    }
}

fn collect_undefined_value_ids(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut ids = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if matches!(
                instr.value,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    ..
                }
            ) {
                ids.insert(instr.lvalue.identifier.id);
            }
        }
    }
    ids
}

fn collect_locally_declared_names(func: &HIRFunction) -> HashSet<String> {
    let mut names = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign
                        && let Some(name) = identifier_name_str(&lvalue.place.identifier)
                    {
                        names.insert(name.to_string());
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign {
                        collect_pattern_names(&lvalue.pattern, &mut names);
                    }
                }
                _ => {}
            }
        }
    }
    names
}

fn collect_pattern_bindings(pattern: &Pattern, bindings: &mut HashMap<String, Identifier>) {
    match pattern {
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if let Some(name) = identifier_name_str(&property.place.identifier) {
                            bindings.insert(name.to_string(), property.place.identifier.clone());
                        }
                        if let Some(key_name) = object_property_key_name(&property.key) {
                            bindings.insert(key_name, property.place.identifier.clone());
                        }
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if let Some(name) = identifier_name_str(&place.identifier) {
                            bindings.insert(name.to_string(), place.identifier.clone());
                        }
                    }
                }
            }
        }
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        if let Some(name) = identifier_name_str(&place.identifier) {
                            bindings.insert(name.to_string(), place.identifier.clone());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
    }
}

fn collect_pattern_names(pattern: &Pattern, names: &mut HashSet<String>) {
    match pattern {
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if let Some(name) = identifier_name_str(&property.place.identifier) {
                            names.insert(name.to_string());
                        }
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if let Some(name) = identifier_name_str(&place.identifier) {
                            names.insert(name.to_string());
                        }
                    }
                }
            }
        }
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        if let Some(name) = identifier_name_str(&place.identifier) {
                            names.insert(name.to_string());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
    }
}

fn object_property_key_name(key: &ObjectPropertyKey) -> Option<String> {
    match key {
        ObjectPropertyKey::String(name) | ObjectPropertyKey::Identifier(name) => {
            Some(name.to_string())
        }
        ObjectPropertyKey::Number(value) => Some(value.to_string()),
        ObjectPropertyKey::Computed(_) => None,
    }
}

fn identifier_name_str(identifier: &Identifier) -> Option<&str> {
    match identifier.name.as_ref()? {
        IdentifierName::Named(name) | IdentifierName::Promoted(name) => Some(name.as_str()),
    }
}

fn source_loc_position(loc: &SourceLocation) -> (u32, u32) {
    match loc {
        SourceLocation::Source(range) => (range.start.line, range.start.column),
        SourceLocation::Generated => (0, 0),
    }
}

fn strip_generated_binding_suffix(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('_')?;
    if base.is_empty() || suffix.is_empty() {
        return None;
    }
    if suffix.chars().all(|ch| ch.is_ascii_digit()) {
        Some(base)
    } else {
        None
    }
}

fn lookup_local_binding_by_capture_name<'a>(
    local_bindings: &'a HashMap<String, Identifier>,
    captured_name: &str,
    visible_before: Option<(u32, u32)>,
) -> Option<&'a Identifier> {
    let mut best_match: Option<&Identifier> = None;
    let mut best_loc: (u32, u32) = (0, 0);

    for (local_name, ident) in local_bindings {
        let name_matches = if local_name == captured_name {
            true
        } else {
            strip_generated_binding_suffix(local_name).is_some_and(|base| base == captured_name)
        };
        if !name_matches {
            continue;
        }
        let loc = source_loc_position(&ident.loc);
        if let Some(upper_bound) = visible_before
            && loc > upper_bound
        {
            continue;
        }
        if best_match.is_none() || loc >= best_loc {
            best_match = Some(ident);
            best_loc = loc;
        }
    }

    best_match
}

fn maybe_rebind_captured_local(
    place: &mut Place,
    local_bindings: &HashMap<String, Identifier>,
    locally_declared_names: &HashSet<String>,
    visible_before: Option<(u32, u32)>,
) {
    let Some(name) = identifier_name_str(&place.identifier) else {
        return;
    };

    if locally_declared_names.contains(name) {
        return;
    }

    if let Some(outer_identifier) =
        lookup_local_binding_by_capture_name(local_bindings, name, visible_before)
    {
        place.identifier = outer_identifier.clone();
    }
}

/// Rewrite return terminals in the inlined body:
/// - store return value into `return_value`
/// - goto `return_target`
fn rewrite_block(
    env: &crate::environment::Environment,
    block: &mut BasicBlock,
    return_target: BlockId,
    return_value: &Place,
) {
    let Terminal::Return { value, loc, .. } = &block.terminal else {
        return;
    };

    let return_operand = value.clone();
    let return_loc = loc.clone();

    block.instructions.push(Instruction {
        id: make_instruction_id(0),
        loc: return_loc.clone(),
        lvalue: create_temporary_place(env, &return_loc),
        value: InstructionValue::StoreLocal {
            lvalue: LValue {
                kind: InstructionKind::Reassign,
                place: return_value.clone(),
            },
            value: return_operand,
            loc: return_loc.clone(),
        },
        effects: None,
    });

    block.terminal = Terminal::Goto {
        block: return_target,
        id: make_instruction_id(0),
        variant: GotoVariant::Break,
        loc: return_loc,
    };
}

fn declare_temporary(
    env: &crate::environment::Environment,
    block: &mut BasicBlock,
    result: &Place,
) {
    block.instructions.push(Instruction {
        id: make_instruction_id(0),
        loc: SourceLocation::Generated,
        lvalue: create_temporary_place(env, &result.loc),
        value: InstructionValue::DeclareLocal {
            lvalue: LValue {
                place: result.clone(),
                kind: InstructionKind::Let,
            },
            loc: result.loc.clone(),
        },
        effects: None,
    });
}

fn create_temporary_place(env: &crate::environment::Environment, loc: &SourceLocation) -> Place {
    Place {
        identifier: env.make_temporary_identifier(loc.clone()),
        effect: Effect::Unknown,
        reactive: false,
        loc: SourceLocation::Generated,
    }
}

fn promote_temporary(identifier: &mut Identifier) {
    debug_assert!(
        identifier.name.is_none(),
        "Expected a temporary (unnamed) identifier"
    );
    if identifier.name.is_none() {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
}

fn terminal_loc(terminal: &Terminal) -> SourceLocation {
    match terminal {
        Terminal::Unsupported { loc, .. }
        | Terminal::Unreachable { loc, .. }
        | Terminal::Throw { loc, .. }
        | Terminal::Return { loc, .. }
        | Terminal::Goto { loc, .. }
        | Terminal::If { loc, .. }
        | Terminal::Branch { loc, .. }
        | Terminal::Switch { loc, .. }
        | Terminal::For { loc, .. }
        | Terminal::ForOf { loc, .. }
        | Terminal::ForIn { loc, .. }
        | Terminal::DoWhile { loc, .. }
        | Terminal::While { loc, .. }
        | Terminal::Logical { loc, .. }
        | Terminal::Ternary { loc, .. }
        | Terminal::Optional { loc, .. }
        | Terminal::Label { loc, .. }
        | Terminal::Sequence { loc, .. }
        | Terminal::Try { loc, .. }
        | Terminal::Scope { loc, .. }
        | Terminal::PrunedScope { loc, .. } => loc.clone(),
    }
}
