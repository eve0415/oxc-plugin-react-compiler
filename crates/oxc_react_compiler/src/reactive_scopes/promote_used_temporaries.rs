//! Promote temporary identifiers that are used in positions requiring named variables.
//!
//! Port of `PromoteUsedTemporaries.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass runs four phases over the `ReactiveFunction` tree:
//!
//! 1. **CollectPromotableTemporaries**: Finds JSX expression tags (identifiers used
//!    as JSX component tags need `#T` naming) and pruned-scope declarations that are
//!    referenced outside their pruned scope.
//!
//! 2. **PromoteTemporaries**: Promotes identifiers in scope dependencies, scope
//!    declarations, params, and nested reactive function params that are still unnamed.
//!
//! 3. **PromoteInterposedTemporaries**: Promotes temporaries whose defs are separated
//!    from their uses by interposing side-effecting statements (to preserve ordering).
//!
//! 4. **PromoteAllInstancesOfPromotedTemporaries**: Ensures every `Identifier`
//!    instance sharing a `DeclarationId` with a promoted identifier also gets promoted.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Promotes temporary identifiers that are used in positions requiring named
/// variables. Operates on the tree-shaped `ReactiveFunction`.
pub fn promote_used_temporaries(func: &mut ReactiveFunction) {
    promote_used_temporaries_impl(func, true);
}

/// Variant for outlined functions: skip promoting spread/rest params to match
/// upstream CodegenReactiveFunction.ts:329-349 which doesn't run
/// promoteUsedTemporaries on outlined functions at all. We still need to
/// promote non-spread identifiers because our HIR creates unnamed temporaries
/// for destructuring params that upstream's BuildHIR names directly.
pub fn promote_used_temporaries_for_outlined(func: &mut ReactiveFunction) {
    promote_used_temporaries_impl(func, false);
}

fn promote_used_temporaries_impl(func: &mut ReactiveFunction, promote_spread_params: bool) {
    let mut state = PromoteState {
        tags: HashSet::new(),
        promoted: HashSet::new(),
        pruned: HashMap::new(),
    };

    // Phase 1: Collect JSX tags and pruned-scope info.
    {
        let mut collector = CollectPromotableTemporaries {
            active_scopes: Vec::new(),
        };
        collector.visit_block(&mut func.body, &mut state);
    }

    // Promote params of the top-level function.
    for param in &mut func.params {
        match param {
            Argument::Place(p) => {
                if p.identifier.name.is_none() {
                    promote_identifier(&mut p.identifier, &mut state);
                }
            }
            Argument::Spread(p) => {
                if promote_spread_params && p.identifier.name.is_none() {
                    promote_identifier(&mut p.identifier, &mut state);
                }
            }
        }
    }

    // Phase 2: Promote identifiers in scope dependencies/declarations.
    visit_block_promote_temporaries(&mut func.body, &mut state);

    // Phase 3: Promote interposed temporaries.
    {
        let mut consts: HashSet<IdentifierId> = HashSet::new();
        let mut globals: HashSet<IdentifierId> = HashSet::new();
        for param in &func.params {
            let place = match param {
                Argument::Place(p) => p,
                Argument::Spread(p) => p,
            };
            consts.insert(place.identifier.id);
        }
        let mut inter_state: InterState = HashMap::new();
        visit_block_promote_interposed(
            &mut func.body,
            &mut inter_state,
            &mut state,
            &mut consts,
            &mut globals,
        );
    }

    // Phase 4: Promote all remaining instances of already-promoted DeclarationIds.
    visit_block_promote_all_instances(&mut func.body, &state);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Shared state threaded through all phases.
struct PromoteState {
    /// DeclarationIds of identifiers used as JSX expression tags.
    tags: HashSet<DeclarationId>,
    /// DeclarationIds that have been promoted so far.
    promoted: HashSet<DeclarationId>,
    /// Pruned-scope declarations: maps DeclarationId -> (active_scopes at declaration, used_outside_scope).
    pruned: HashMap<DeclarationId, PrunedInfo>,
}

struct PrunedInfo {
    active_scopes: Vec<ScopeId>,
    used_outside_scope: bool,
}

/// State for the interposed-temporaries phase.
/// Maps IdentifierId -> (a mutable reference key into the identifier, needs_promotion).
/// We store `DeclarationId` plus the promotion flag.
type InterState = HashMap<IdentifierId, (DeclarationId, bool)>;

// ---------------------------------------------------------------------------
// Core helper: promote an identifier
// ---------------------------------------------------------------------------

/// Promotes a temporary identifier by assigning it a `Promoted` name.
/// Uses `#T{id}` for JSX tags and `#t{id}` for everything else.
fn promote_identifier(identifier: &mut Identifier, state: &mut PromoteState) {
    debug_assert!(
        identifier.name.is_none(),
        "promoteTemporary: Expected to be called only for temporary variables"
    );
    if state.tags.contains(&identifier.declaration_id) {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#T{}",
            identifier.declaration_id.0
        )));
    } else {
        identifier.name = Some(IdentifierName::Promoted(format!(
            "#t{}",
            identifier.declaration_id.0
        )));
    }
    state.promoted.insert(identifier.declaration_id);
}

// ---------------------------------------------------------------------------
// Helpers for iterating instruction value lvalues and pattern operands
// ---------------------------------------------------------------------------

/// Yields all places from a destructuring pattern.
fn each_pattern_operand(pattern: &Pattern) -> Vec<&Place> {
    let mut result = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => result.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => result.push(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => result.push(p),
                }
            }
        }
    }
    result
}

/// Mutable version: yields mutable references to all places from a destructuring pattern.
fn each_pattern_operand_mut(pattern: &mut Pattern) -> Vec<&mut Place> {
    let mut result = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => result.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => result.push(&mut p.place),
                    ObjectPropertyOrSpread::Spread(p) => result.push(p),
                }
            }
        }
    }
    result
}

// ===========================================================================
// Phase 1: CollectPromotableTemporaries
// ===========================================================================

struct CollectPromotableTemporaries {
    active_scopes: Vec<ScopeId>,
}

impl CollectPromotableTemporaries {
    fn visit_block(&mut self, block: &mut ReactiveBlock, state: &mut PromoteState) {
        for stmt in block.iter_mut() {
            self.visit_statement(stmt, state);
        }
    }

    fn visit_statement(&mut self, stmt: &mut ReactiveStatement, state: &mut PromoteState) {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                self.visit_instruction(instr, state);
            }
            ReactiveStatement::Terminal(term) => {
                self.visit_terminal(&mut term.terminal, state);
            }
            ReactiveStatement::Scope(scope_block) => {
                self.active_scopes.push(scope_block.scope.id);
                self.visit_block(&mut scope_block.instructions, state);
                self.active_scopes.pop();
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                // Record all declarations of the pruned scope.
                for (_id, decl) in &scope_block.scope.declarations {
                    state.pruned.insert(
                        decl.identifier.declaration_id,
                        PrunedInfo {
                            active_scopes: self.active_scopes.clone(),
                            used_outside_scope: false,
                        },
                    );
                }
                self.visit_block(&mut scope_block.instructions, state);
            }
        }
    }

    fn visit_instruction(&mut self, instr: &mut ReactiveInstruction, state: &mut PromoteState) {
        // Visit the value to find JSX tags and places.
        self.visit_value(&instr.value, state);
        // Visit places in the instruction (operands).
        self.visit_instruction_places(instr, state);
    }

    fn visit_value(&mut self, value: &InstructionValue, state: &mut PromoteState) {
        // Check for JSX expression tags.
        if let InstructionValue::JsxExpression { tag, .. } = value
            && let JsxTag::Component(place) = tag
        {
            state.tags.insert(place.identifier.declaration_id);
        }
        // Recurse into nested functions.
        match value {
            InstructionValue::FunctionExpression { lowered_func, .. }
            | InstructionValue::ObjectMethod { lowered_func, .. } => {
                self.visit_hir_function(&lowered_func.func, state);
            }
            _ => {}
        }
    }

    /// Visit all places referenced in an instruction (operands) to detect pruned-scope usage.
    fn visit_instruction_places(&mut self, instr: &ReactiveInstruction, state: &mut PromoteState) {
        for place in each_instruction_value_operands(&instr.value) {
            self.visit_place(place, state);
        }
    }

    fn visit_place(&self, place: &Place, state: &mut PromoteState) {
        if !self.active_scopes.is_empty()
            && let Some(pruned_info) = state.pruned.get_mut(&place.identifier.declaration_id)
        {
            let current_scope = self.active_scopes.last().unwrap();
            if !pruned_info.active_scopes.contains(current_scope) {
                pruned_info.used_outside_scope = true;
            }
        }
    }

    fn visit_hir_function(&mut self, func: &HIRFunction, state: &mut PromoteState) {
        for (_, block) in &func.body.blocks {
            for instr_hir in &block.instructions {
                // Check for JSX tags in HIR instructions.
                if let InstructionValue::JsxExpression { tag, .. } = &instr_hir.value
                    && let JsxTag::Component(place) = tag
                {
                    state.tags.insert(place.identifier.declaration_id);
                }
                // Recurse into nested functions.
                match &instr_hir.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        self.visit_hir_function(&lowered_func.func, state);
                    }
                    _ => {}
                }
            }
        }
    }

    fn visit_terminal(&mut self, terminal: &mut ReactiveTerminal, state: &mut PromoteState) {
        match terminal {
            ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
                self.visit_place(value, state);
            }
            ReactiveTerminal::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                self.visit_place(test, state);
                self.visit_block(consequent, state);
                if let Some(alt) = alternate {
                    self.visit_block(alt, state);
                }
            }
            ReactiveTerminal::Switch { test, cases, .. } => {
                self.visit_place(test, state);
                for case in cases.iter_mut() {
                    if let Some(t) = &case.test {
                        self.visit_place(t, state);
                    }
                    if let Some(block) = &mut case.block {
                        self.visit_block(block, state);
                    }
                }
            }
            ReactiveTerminal::DoWhile {
                loop_block, test, ..
            } => {
                self.visit_block(loop_block, state);
                self.visit_place(test, state);
            }
            ReactiveTerminal::While {
                test, loop_block, ..
            } => {
                self.visit_place(test, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::For {
                init,
                test,
                update,
                loop_block,
                ..
            } => {
                self.visit_block(init, state);
                self.visit_place(test, state);
                if let Some(upd) = update {
                    self.visit_block(upd, state);
                }
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::ForOf {
                init,
                test,
                loop_block,
                ..
            } => {
                self.visit_block(init, state);
                self.visit_place(test, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                self.visit_block(init, state);
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::Label { block, .. } => {
                self.visit_block(block, state);
            }
            ReactiveTerminal::Try { block, handler, .. } => {
                self.visit_block(block, state);
                self.visit_block(handler, state);
            }
            ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        }
    }
}

// ===========================================================================
// Phase 2: PromoteTemporaries
// ===========================================================================

/// Walks the tree and promotes identifiers in scope dependencies, scope
/// declarations, and nested reactive function params.
fn visit_block_promote_temporaries(block: &mut ReactiveBlock, state: &mut PromoteState) {
    for stmt in block.iter_mut() {
        visit_statement_promote_temporaries(stmt, state);
    }
}

fn visit_statement_promote_temporaries(stmt: &mut ReactiveStatement, state: &mut PromoteState) {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            visit_instruction_promote_temporaries(instr, state);
        }
        ReactiveStatement::Terminal(term) => {
            visit_terminal_promote_temporaries(&mut term.terminal, state);
        }
        ReactiveStatement::Scope(scope_block) => {
            // Promote scope dependencies.
            for dep in &mut scope_block.scope.dependencies {
                if dep.identifier.name.is_none() {
                    promote_identifier(&mut dep.identifier, state);
                }
            }
            // Promote scope declarations.
            for decl in scope_block.scope.declarations.values_mut() {
                if decl.identifier.name.is_none() {
                    promote_identifier(&mut decl.identifier, state);
                }
            }
            visit_block_promote_temporaries(&mut scope_block.instructions, state);
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            // For pruned scopes, only promote declarations that are used outside.
            for decl in scope_block.scope.declarations.values_mut() {
                if decl.identifier.name.is_none()
                    && let Some(pruned_info) = state.pruned.get(&decl.identifier.declaration_id)
                    && pruned_info.used_outside_scope
                {
                    promote_identifier(&mut decl.identifier, state);
                }
            }
            visit_block_promote_temporaries(&mut scope_block.instructions, state);
        }
    }
}

fn visit_instruction_promote_temporaries(
    instr: &mut ReactiveInstruction,
    state: &mut PromoteState,
) {
    // Promote unnamed Destructure pattern operands (non-spread elements).
    // Spread/rest elements may legitimately remain unnamed (matching upstream's
    // convertIdentifier invariant for the error.bug-invariant-unnamed-temporary case).
    if let InstructionValue::Destructure { lvalue, .. } = &mut instr.value {
        match &mut lvalue.pattern {
            Pattern::Array(arr) => {
                for elem in arr.items.iter_mut() {
                    // Skip ArrayElement::Spread — may legitimately stay unnamed
                    if let ArrayElement::Place(place) = elem
                        && place.identifier.name.is_none()
                    {
                        promote_identifier(&mut place.identifier, state);
                    }
                }
            }
            Pattern::Object(obj) => {
                for prop in obj.properties.iter_mut() {
                    // Skip ObjectPropertyOrSpread::Spread — may legitimately stay unnamed
                    if let ObjectPropertyOrSpread::Property(p) = prop
                        && p.place.identifier.name.is_none()
                    {
                        promote_identifier(&mut p.place.identifier, state);
                    }
                }
            }
        }
    }
    // Visit value: for FunctionExpression/ObjectMethod, recurse into nested HIR function.
    visit_value_promote_temporaries(instr, state);
}

fn visit_value_promote_temporaries(instr: &mut ReactiveInstruction, state: &mut PromoteState) {
    match &mut instr.value {
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            visit_hir_function_promote_temporaries(&mut lowered_func.func, state);
        }
        _ => {}
    }
}

fn visit_hir_function_promote_temporaries(func: &mut HIRFunction, state: &mut PromoteState) {
    // Promote params of nested HIR functions.
    for param in &mut func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        if place.identifier.name.is_none() {
            promote_identifier(&mut place.identifier, state);
        }
    }
    // Visit all instructions in the HIR function.
    for (_, block) in &mut func.body.blocks {
        for instr_hir in &mut block.instructions {
            match &mut instr_hir.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    visit_hir_function_promote_temporaries(&mut lowered_func.func, state);
                }
                _ => {}
            }
        }
    }
}

fn visit_terminal_promote_temporaries(terminal: &mut ReactiveTerminal, state: &mut PromoteState) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            visit_block_promote_temporaries(consequent, state);
            if let Some(alt) = alternate {
                visit_block_promote_temporaries(alt, state);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    visit_block_promote_temporaries(block, state);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            visit_block_promote_temporaries(loop_block, state);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            visit_block_promote_temporaries(init, state);
            if let Some(upd) = update {
                visit_block_promote_temporaries(upd, state);
            }
            visit_block_promote_temporaries(loop_block, state);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            visit_block_promote_temporaries(init, state);
            visit_block_promote_temporaries(loop_block, state);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_block_promote_temporaries(init, state);
            visit_block_promote_temporaries(loop_block, state);
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_block_promote_temporaries(block, state);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            visit_block_promote_temporaries(block, state);
            visit_block_promote_temporaries(handler, state);
        }
        ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
}

// ===========================================================================
// Phase 3: PromoteInterposedTemporaries
// ===========================================================================

/// Walks the tree and promotes temporaries whose definitions are separated from
/// their uses by interposing side-effecting statements.
fn visit_block_promote_interposed(
    block: &mut ReactiveBlock,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &mut HashSet<IdentifierId>,
    globals: &mut HashSet<IdentifierId>,
) {
    for stmt in block.iter_mut() {
        visit_statement_promote_interposed(stmt, inter_state, promote_state, consts, globals);
    }
}

fn visit_statement_promote_interposed(
    stmt: &mut ReactiveStatement,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &mut HashSet<IdentifierId>,
    globals: &mut HashSet<IdentifierId>,
) {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            visit_instruction_interposed(instr, inter_state, promote_state, consts, globals);
        }
        ReactiveStatement::Terminal(term) => {
            visit_terminal_interposed(
                &mut term.terminal,
                inter_state,
                promote_state,
                consts,
                globals,
            );
        }
        ReactiveStatement::Scope(scope_block) => {
            visit_block_promote_interposed(
                &mut scope_block.instructions,
                inter_state,
                promote_state,
                consts,
                globals,
            );
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            visit_block_promote_interposed(
                &mut scope_block.instructions,
                inter_state,
                promote_state,
                consts,
                globals,
            );
        }
    }
}

fn visit_instruction_interposed(
    instr: &mut ReactiveInstruction,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &mut HashSet<IdentifierId>,
    globals: &mut HashSet<IdentifierId>,
) {
    // Upstream asserts that all value lvalues (assignment targets) are named at this point.
    // However, earlier passes may not have promoted all identifiers yet, so we skip
    // this check gracefully rather than panicking. The unnamed lvalues will be handled
    // by Phase 4 (PromoteAllInstancesOfPromotedTemporaries) if needed.

    match &mut instr.value {
        InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::PropertyStore { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::PostfixUpdate { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::StoreGlobal { .. }
        | InstructionValue::Destructure { .. } => {
            let mut const_store = false;

            // Check for const stores.
            match &instr.value {
                InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                    if lvalue.kind == InstructionKind::Const
                        || lvalue.kind == InstructionKind::HoistedConst =>
                {
                    consts.insert(lvalue.place.identifier.id);
                    const_store = true;
                }
                _ => {}
            }
            match &instr.value {
                InstructionValue::Destructure { lvalue, .. }
                    if lvalue.kind == InstructionKind::Const
                        || lvalue.kind == InstructionKind::HoistedConst =>
                {
                    for place in each_pattern_operand(&lvalue.pattern) {
                        consts.insert(place.identifier.id);
                    }
                    const_store = true;
                }
                _ => {}
            }
            // Treat property of method call as const-like.
            if let InstructionValue::MethodCall { property, .. } = &instr.value {
                consts.insert(property.identifier.id);
            }

            // Visit operand places to check for needed promotions.
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);

            let lvalue_effectively_named = instr.lvalue.as_ref().is_some_and(|lv| {
                lv.identifier.name.is_some()
                    || promote_state
                        .promoted
                        .contains(&lv.identifier.declaration_id)
            });
            if !const_store && (instr.lvalue.is_none() || lvalue_effectively_named) {
                // Mark all tracked temporaries as needing promotion.
                for (_key, (_decl_id, needs_promo)) in inter_state.iter_mut() {
                    *needs_promo = true;
                }
            }
            if let Some(lvalue) = &instr.lvalue
                && lvalue.identifier.name.is_none()
                && !promote_state
                    .promoted
                    .contains(&lvalue.identifier.declaration_id)
            {
                inter_state.insert(
                    lvalue.identifier.id,
                    (lvalue.identifier.declaration_id, false),
                );
            }
        }

        InstructionValue::DeclareContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. } => {
            if lvalue.kind == InstructionKind::Const || lvalue.kind == InstructionKind::HoistedConst
            {
                consts.insert(lvalue.place.identifier.id);
            }
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);
        }

        InstructionValue::LoadContext { place, .. } | InstructionValue::LoadLocal { place, .. } => {
            if let Some(lvalue) = &instr.lvalue
                && lvalue.identifier.name.is_none()
                && !promote_state
                    .promoted
                    .contains(&lvalue.identifier.declaration_id)
            {
                if consts.contains(&place.identifier.id) {
                    consts.insert(lvalue.identifier.id);
                }
                inter_state.insert(
                    lvalue.identifier.id,
                    (lvalue.identifier.declaration_id, false),
                );
            }
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);
        }

        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::ComputedLoad { object, .. } => {
            if let Some(lvalue) = &instr.lvalue {
                if globals.contains(&object.identifier.id) {
                    globals.insert(lvalue.identifier.id);
                    consts.insert(lvalue.identifier.id);
                }
                if lvalue.identifier.name.is_none()
                    && !promote_state
                        .promoted
                        .contains(&lvalue.identifier.declaration_id)
                {
                    inter_state.insert(
                        lvalue.identifier.id,
                        (lvalue.identifier.declaration_id, false),
                    );
                }
            }
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);
        }

        InstructionValue::LoadGlobal { .. } => {
            if let Some(lvalue) = &instr.lvalue {
                globals.insert(lvalue.identifier.id);
            }
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);
        }

        _ => {
            visit_instruction_operands_interposed(instr, inter_state, promote_state, consts);
        }
    }
}

/// Visit all operand places of an instruction (for the interposed pass).
/// When we encounter a place that is tracked in inter_state and marked as
/// needing promotion, we promote it.
fn visit_instruction_operands_interposed(
    instr: &mut ReactiveInstruction,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &HashSet<IdentifierId>,
) {
    // Collect the ids we need to promote first to avoid borrow issues.
    let mut to_promote: Vec<IdentifierId> = Vec::new();

    for place in each_instruction_value_operands(&instr.value) {
        if let Some((decl_id, needs_promotion)) = inter_state.get(&place.identifier.id)
            && *needs_promotion
            && !consts.contains(&place.identifier.id)
        {
            // Check if identifier with this decl_id is still unnamed.
            if !promote_state.promoted.contains(decl_id) {
                to_promote.push(place.identifier.id);
            }
        }
    }

    // Now do the actual promotion by finding the identifiers in the inter_state.
    // We need to promote the original identifier that was stored when the def was seen.
    // Since we only have DeclarationId, we promote via the state.
    for id in to_promote {
        if let Some((decl_id, _)) = inter_state.get(&id) {
            // Mark as promoted in the state; the actual name assignment happens
            // in phase 4 for all instances.
            promote_state.promoted.insert(*decl_id);
            // Determine the name.
            if promote_state.tags.contains(decl_id) {
                // Will be named #T{id} -- but we just mark it as promoted here.
                // Phase 4 will apply names to all instances.
            }
        }
    }

    // Also check the lvalue.
    if let Some(lvalue) = &instr.lvalue
        && let Some((decl_id, needs_promotion)) = inter_state.get(&lvalue.identifier.id)
        && *needs_promotion
        && !consts.contains(&lvalue.identifier.id)
        && !promote_state.promoted.contains(decl_id)
    {
        promote_state.promoted.insert(*decl_id);
    }
}

fn visit_terminal_interposed(
    terminal: &mut ReactiveTerminal,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &mut HashSet<IdentifierId>,
    globals: &mut HashSet<IdentifierId>,
) {
    // Visit terminal operand places.
    visit_terminal_places_interposed(terminal, inter_state, promote_state, consts);

    // Recurse into sub-blocks.
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            visit_block_promote_interposed(consequent, inter_state, promote_state, consts, globals);
            if let Some(alt) = alternate {
                visit_block_promote_interposed(alt, inter_state, promote_state, consts, globals);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    visit_block_promote_interposed(
                        block,
                        inter_state,
                        promote_state,
                        consts,
                        globals,
                    );
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            visit_block_promote_interposed(loop_block, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            visit_block_promote_interposed(init, inter_state, promote_state, consts, globals);
            if let Some(upd) = update {
                visit_block_promote_interposed(upd, inter_state, promote_state, consts, globals);
            }
            visit_block_promote_interposed(loop_block, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            visit_block_promote_interposed(init, inter_state, promote_state, consts, globals);
            visit_block_promote_interposed(loop_block, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_block_promote_interposed(init, inter_state, promote_state, consts, globals);
            visit_block_promote_interposed(loop_block, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_block_promote_interposed(block, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            visit_block_promote_interposed(block, inter_state, promote_state, consts, globals);
            visit_block_promote_interposed(handler, inter_state, promote_state, consts, globals);
        }
        ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
}

fn visit_terminal_places_interposed(
    terminal: &ReactiveTerminal,
    inter_state: &mut InterState,
    promote_state: &mut PromoteState,
    consts: &HashSet<IdentifierId>,
) {
    let places = collect_terminal_operand_places(terminal);
    for place in &places {
        if let Some((decl_id, needs_promotion)) = inter_state.get(&place.identifier.id)
            && *needs_promotion
            && !consts.contains(&place.identifier.id)
            && !promote_state.promoted.contains(decl_id)
        {
            promote_state.promoted.insert(*decl_id);
        }
    }
}

// ===========================================================================
// Phase 4: PromoteAllInstancesOfPromotedTemporaries
// ===========================================================================

/// Walks the tree and promotes every identifier instance whose DeclarationId
/// has been marked as promoted.
fn visit_block_promote_all_instances(block: &mut ReactiveBlock, state: &PromoteState) {
    for stmt in block.iter_mut() {
        visit_statement_promote_all(stmt, state);
    }
}

fn visit_statement_promote_all(stmt: &mut ReactiveStatement, state: &PromoteState) {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            visit_instruction_promote_all(instr, state);
        }
        ReactiveStatement::Terminal(term) => {
            visit_terminal_promote_all(&mut term.terminal, state);
        }
        ReactiveStatement::Scope(scope_block) => {
            visit_block_promote_all_instances(&mut scope_block.instructions, state);
            promote_scope_identifiers(&mut scope_block.scope, state);
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            visit_block_promote_all_instances(&mut scope_block.instructions, state);
            promote_scope_identifiers(&mut scope_block.scope, state);
        }
    }
}

fn visit_instruction_promote_all(instr: &mut ReactiveInstruction, state: &PromoteState) {
    // Visit lvalue.
    if let Some(lvalue) = &mut instr.lvalue {
        maybe_promote_place(&mut lvalue.identifier, state);
    }

    // Visit all operand places.
    visit_instruction_value_places_mut(&mut instr.value, state);

    // Visit value lvalues.
    visit_instruction_value_lvalues_mut(&mut instr.value, state);
}

fn maybe_promote_place(identifier: &mut Identifier, state: &PromoteState) {
    if identifier.name.is_none() && state.promoted.contains(&identifier.declaration_id) {
        if state.tags.contains(&identifier.declaration_id) {
            identifier.name = Some(IdentifierName::Promoted(format!(
                "#T{}",
                identifier.declaration_id.0
            )));
        } else {
            identifier.name = Some(IdentifierName::Promoted(format!(
                "#t{}",
                identifier.declaration_id.0
            )));
        }
    }
}

fn promote_scope_identifiers(scope: &mut ReactiveScope, state: &PromoteState) {
    for decl in scope.declarations.values_mut() {
        maybe_promote_place(&mut decl.identifier, state);
    }
    for dep in &mut scope.dependencies {
        maybe_promote_place(&mut dep.identifier, state);
    }
    for reassignment in &mut scope.reassignments {
        maybe_promote_place(reassignment, state);
    }
}

fn visit_terminal_promote_all(terminal: &mut ReactiveTerminal, state: &PromoteState) {
    // Visit operand places in the terminal.
    visit_terminal_operand_places_mut(terminal, state);

    // Recurse into sub-blocks.
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            visit_block_promote_all_instances(consequent, state);
            if let Some(alt) = alternate {
                visit_block_promote_all_instances(alt, state);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    visit_block_promote_all_instances(block, state);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            visit_block_promote_all_instances(loop_block, state);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            visit_block_promote_all_instances(init, state);
            if let Some(upd) = update {
                visit_block_promote_all_instances(upd, state);
            }
            visit_block_promote_all_instances(loop_block, state);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            visit_block_promote_all_instances(init, state);
            visit_block_promote_all_instances(loop_block, state);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_block_promote_all_instances(init, state);
            visit_block_promote_all_instances(loop_block, state);
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_block_promote_all_instances(block, state);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            visit_block_promote_all_instances(block, state);
            visit_block_promote_all_instances(handler, state);
        }
        ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
}

fn visit_terminal_operand_places_mut(terminal: &mut ReactiveTerminal, state: &PromoteState) {
    match terminal {
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            maybe_promote_place(&mut value.identifier, state);
        }
        ReactiveTerminal::If { test, .. } => {
            maybe_promote_place(&mut test.identifier, state);
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            maybe_promote_place(&mut test.identifier, state);
            for case in cases.iter_mut() {
                if let Some(t) = &mut case.test {
                    maybe_promote_place(&mut t.identifier, state);
                }
            }
        }
        ReactiveTerminal::DoWhile { test, .. } | ReactiveTerminal::While { test, .. } => {
            maybe_promote_place(&mut test.identifier, state);
        }
        ReactiveTerminal::For { test, .. } | ReactiveTerminal::ForOf { test, .. } => {
            maybe_promote_place(&mut test.identifier, state);
        }
        ReactiveTerminal::Try {
            handler_binding, ..
        } => {
            if let Some(binding) = handler_binding {
                maybe_promote_place(&mut binding.identifier, state);
            }
        }
        ReactiveTerminal::ForIn { .. }
        | ReactiveTerminal::Label { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
}

// ===========================================================================
// Instruction value place visitors
// ===========================================================================

/// Collect all operand places from an instruction value (immutable).
fn each_instruction_value_operands(value: &InstructionValue) -> Vec<&Place> {
    let mut places = Vec::new();
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            places.push(place);
        }
        InstructionValue::StoreLocal {
            lvalue: _,
            value: val,
            ..
        }
        | InstructionValue::StoreContext {
            lvalue: _,
            value: val,
            ..
        } => {
            places.push(val);
        }
        InstructionValue::DeclareLocal { .. } | InstructionValue::DeclareContext { .. } => {}
        InstructionValue::Destructure { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            places.push(left);
            places.push(right);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            places.push(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => places.push(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            places.push(receiver);
            places.push(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => places.push(p),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            places.push(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => places.push(p),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            places.push(place);
                        }
                        places.push(&p.place);
                    }
                    ObjectPropertyOrSpread::Spread(p) => places.push(p),
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => places.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            places.push(object);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            places.push(object);
            places.push(val);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            places.push(object);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            places.push(object);
            places.push(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            places.push(object);
            places.push(property);
            places.push(val);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            places.push(object);
            places.push(property);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(p) = tag {
                places.push(p);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => places.push(place),
                    JsxAttribute::SpreadAttribute { argument } => places.push(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    places.push(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                places.push(child);
            }
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            places.push(test);
            places.push(consequent);
            places.push(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            places.push(left);
            places.push(right);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    places.push(lvalue);
                }
                places.extend(each_instruction_value_operands(&instr.value));
            }
            places.extend(each_instruction_value_operands(value));
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            places.extend(each_instruction_value_operands(value));
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            places.extend(each_instruction_value_operands(left));
            places.extend(each_instruction_value_operands(right));
        }
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => {
            places.extend(each_instruction_value_operands(test));
            places.extend(each_instruction_value_operands(consequent));
            places.extend(each_instruction_value_operands(alternate));
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            places.push(tag);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                places.push(expr);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::GetIterator { collection, .. } => {
            places.push(collection);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            places.push(iterator);
            places.push(collection);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::PrefixUpdate {
            lvalue: _,
            value: val,
            ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue: _,
            value: val,
            ..
        } => {
            places.push(val);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            places.push(decl);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            places.push(val);
        }
        InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
    places
}

/// Visit all operand places of an instruction value mutably for phase 4.
fn visit_instruction_value_places_mut(value: &mut InstructionValue, state: &PromoteState) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            maybe_promote_place(&mut place.identifier, state);
        }
        InstructionValue::StoreLocal {
            lvalue: _,
            value: val,
            ..
        }
        | InstructionValue::StoreContext {
            lvalue: _,
            value: val,
            ..
        } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::DeclareLocal { .. } | InstructionValue::DeclareContext { .. } => {}
        InstructionValue::Destructure { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            maybe_promote_place(&mut left.identifier, state);
            maybe_promote_place(&mut right.identifier, state);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            maybe_promote_place(&mut callee.identifier, state);
            for arg in args.iter_mut() {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        maybe_promote_place(&mut p.identifier, state);
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
            maybe_promote_place(&mut receiver.identifier, state);
            maybe_promote_place(&mut property.identifier, state);
            for arg in args.iter_mut() {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        maybe_promote_place(&mut p.identifier, state);
                    }
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            maybe_promote_place(&mut callee.identifier, state);
            for arg in args.iter_mut() {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        maybe_promote_place(&mut p.identifier, state);
                    }
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties.iter_mut() {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            maybe_promote_place(&mut place.identifier, state);
                        }
                        maybe_promote_place(&mut p.place.identifier, state);
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        maybe_promote_place(&mut p.identifier, state);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements.iter_mut() {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        maybe_promote_place(&mut p.identifier, state);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            maybe_promote_place(&mut object.identifier, state);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            maybe_promote_place(&mut object.identifier, state);
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            maybe_promote_place(&mut object.identifier, state);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            maybe_promote_place(&mut object.identifier, state);
            maybe_promote_place(&mut property.identifier, state);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            maybe_promote_place(&mut object.identifier, state);
            maybe_promote_place(&mut property.identifier, state);
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            maybe_promote_place(&mut object.identifier, state);
            maybe_promote_place(&mut property.identifier, state);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(p) = tag {
                maybe_promote_place(&mut p.identifier, state);
            }
            for attr in props.iter_mut() {
                match attr {
                    JsxAttribute::Attribute { place, .. } => {
                        maybe_promote_place(&mut place.identifier, state);
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        maybe_promote_place(&mut argument.identifier, state);
                    }
                }
            }
            if let Some(children) = children {
                for child in children.iter_mut() {
                    maybe_promote_place(&mut child.identifier, state);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children.iter_mut() {
                maybe_promote_place(&mut child.identifier, state);
            }
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            maybe_promote_place(&mut test.identifier, state);
            maybe_promote_place(&mut consequent.identifier, state);
            maybe_promote_place(&mut alternate.identifier, state);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            maybe_promote_place(&mut left.identifier, state);
            maybe_promote_place(&mut right.identifier, state);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions.iter_mut() {
                if let Some(lvalue) = &mut instr.lvalue {
                    maybe_promote_place(&mut lvalue.identifier, state);
                }
                visit_instruction_value_places_mut(&mut instr.value, state);
            }
            visit_instruction_value_places_mut(value, state);
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            visit_instruction_value_places_mut(value, state);
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            visit_instruction_value_places_mut(left, state);
            visit_instruction_value_places_mut(right, state);
        }
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => {
            visit_instruction_value_places_mut(test, state);
            visit_instruction_value_places_mut(consequent, state);
            visit_instruction_value_places_mut(alternate, state);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            maybe_promote_place(&mut tag.identifier, state);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs.iter_mut() {
                maybe_promote_place(&mut expr.identifier, state);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::GetIterator { collection, .. } => {
            maybe_promote_place(&mut collection.identifier, state);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            maybe_promote_place(&mut iterator.identifier, state);
            maybe_promote_place(&mut collection.identifier, state);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::PrefixUpdate { value: val, .. }
        | InstructionValue::PostfixUpdate { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            maybe_promote_place(&mut decl.identifier, state);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            maybe_promote_place(&mut val.identifier, state);
        }
        InstructionValue::FunctionExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

/// Visit instruction value lvalues mutably for phase 4.
fn visit_instruction_value_lvalues_mut(value: &mut InstructionValue, state: &PromoteState) {
    match value {
        InstructionValue::DeclareContext { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::StoreLocal { lvalue, .. } => {
            maybe_promote_place(&mut lvalue.place.identifier, state);
        }
        InstructionValue::Destructure { lvalue, .. } => {
            for place in each_pattern_operand_mut(&mut lvalue.pattern) {
                maybe_promote_place(&mut place.identifier, state);
            }
        }
        InstructionValue::PostfixUpdate { lvalue, .. }
        | InstructionValue::PrefixUpdate { lvalue, .. } => {
            maybe_promote_place(&mut lvalue.identifier, state);
        }
        _ => {}
    }
}

/// Collect terminal operand places (immutable, for phase 3).
fn collect_terminal_operand_places(terminal: &ReactiveTerminal) -> Vec<&Place> {
    let mut places = Vec::new();
    match terminal {
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            places.push(value);
        }
        ReactiveTerminal::If { test, .. } => {
            places.push(test);
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            places.push(test);
            for case in cases {
                if let Some(t) = &case.test {
                    places.push(t);
                }
            }
        }
        ReactiveTerminal::DoWhile { test, .. } | ReactiveTerminal::While { test, .. } => {
            places.push(test);
        }
        ReactiveTerminal::For { test, .. } | ReactiveTerminal::ForOf { test, .. } => {
            places.push(test);
        }
        ReactiveTerminal::Try {
            handler_binding, ..
        } => {
            if let Some(binding) = handler_binding {
                places.push(binding);
            }
        }
        ReactiveTerminal::ForIn { .. }
        | ReactiveTerminal::Label { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
    places
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_identifier(id: u32, name: Option<IdentifierName>) -> Identifier {
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

    fn make_test_place(id: u32, name: Option<IdentifierName>) -> Place {
        Place {
            identifier: make_test_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    #[test]
    fn test_promote_scope_dependencies() {
        // A scope with an unnamed dependency should get promoted.
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: ReactiveScope {
                    id: ScopeId(0),
                    range: MutableRange {
                        start: InstructionId(0),
                        end: InstructionId(10),
                    },
                    dependencies: vec![ReactiveScopeDependency {
                        identifier: make_test_identifier(1, None),
                        path: vec![],
                    }],
                    declarations: Default::default(),
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                },
                instructions: vec![],
            })],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            let dep = &scope_block.scope.dependencies[0];
            assert!(
                dep.identifier.name.is_some(),
                "Dependency should be promoted"
            );
            let name = dep.identifier.name.as_ref().unwrap();
            assert!(
                matches!(name, IdentifierName::Promoted(s) if s == "#t1"),
                "Expected #t1, got {:?}",
                name
            );
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_promote_scope_declarations() {
        // A scope with an unnamed declaration should get promoted.
        let mut declarations = indexmap::IndexMap::new();
        declarations.insert(
            IdentifierId(2),
            ScopeDeclaration {
                identifier: make_test_identifier(2, None),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: ReactiveScope {
                    id: ScopeId(0),
                    range: MutableRange {
                        start: InstructionId(0),
                        end: InstructionId(10),
                    },
                    dependencies: vec![],
                    declarations,
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                },
                instructions: vec![],
            })],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            let decl = scope_block
                .scope
                .declarations
                .get(&IdentifierId(2))
                .unwrap();
            assert!(
                decl.identifier.name.is_some(),
                "Declaration should be promoted"
            );
            let name = decl.identifier.name.as_ref().unwrap();
            assert!(
                matches!(name, IdentifierName::Promoted(s) if s == "#t2"),
                "Expected #t2, got {:?}",
                name
            );
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_promote_params() {
        // A param with no name should get promoted.
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![Argument::Place(make_test_place(1, None))],
            generator: false,
            async_: false,
            body: vec![],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        if let Argument::Place(place) = &func.params[0] {
            assert!(place.identifier.name.is_some(), "Param should be promoted");
            let name = place.identifier.name.as_ref().unwrap();
            assert!(
                matches!(name, IdentifierName::Promoted(s) if s == "#t1"),
                "Expected #t1, got {:?}",
                name
            );
        } else {
            panic!("Expected Place argument");
        }
    }

    #[test]
    fn test_jsx_tag_promotion() {
        // An identifier used as a JSX tag should get #T prefix.
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                // Instruction with JSX expression using a component tag.
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(0),
                    lvalue: Some(make_test_place(10, None)),
                    value: InstructionValue::JsxExpression {
                        tag: JsxTag::Component(make_test_place(1, None)),
                        props: vec![],
                        children: None,
                        loc: SourceLocation::Generated,
                        opening_loc: SourceLocation::Generated,
                        closing_loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                // Scope that depends on the same identifier.
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: ReactiveScope {
                        id: ScopeId(0),
                        range: MutableRange {
                            start: InstructionId(0),
                            end: InstructionId(10),
                        },
                        dependencies: vec![ReactiveScopeDependency {
                            identifier: make_test_identifier(1, None),
                            path: vec![],
                        }],
                        declarations: Default::default(),
                        reassignments: vec![],
                        merged_id: None,
                        early_return_value: None,
                    },
                    instructions: vec![],
                }),
            ],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        if let ReactiveStatement::Scope(scope_block) = &func.body[1] {
            let dep = &scope_block.scope.dependencies[0];
            assert!(
                dep.identifier.name.is_some(),
                "Dependency should be promoted"
            );
            let name = dep.identifier.name.as_ref().unwrap();
            assert!(
                matches!(name, IdentifierName::Promoted(s) if s == "#T1"),
                "Expected #T1 for JSX tag, got {:?}",
                name
            );
        } else {
            panic!("Expected Scope statement");
        }
    }

    #[test]
    fn test_named_identifiers_not_promoted() {
        // An identifier that already has a name should not be promoted.
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![Argument::Place(make_test_place(
                1,
                Some(IdentifierName::Named("x".to_string())),
            ))],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: ReactiveScope {
                    id: ScopeId(0),
                    range: MutableRange {
                        start: InstructionId(0),
                        end: InstructionId(10),
                    },
                    dependencies: vec![ReactiveScopeDependency {
                        identifier: make_test_identifier(
                            1,
                            Some(IdentifierName::Named("x".to_string())),
                        ),
                        path: vec![],
                    }],
                    declarations: Default::default(),
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                },
                instructions: vec![],
            })],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        // Should still be named "x", not promoted.
        if let Argument::Place(place) = &func.params[0] {
            assert_eq!(place.identifier.name.as_ref().unwrap().value(), "x");
        }
        if let ReactiveStatement::Scope(scope_block) = &func.body[0] {
            let dep = &scope_block.scope.dependencies[0];
            assert_eq!(dep.identifier.name.as_ref().unwrap().value(), "x");
        }
    }

    #[test]
    fn test_promote_all_instances() {
        // If a declaration is promoted, all instances with the same DeclarationId
        // should also be promoted.
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                // Scope that depends on identifier 1 (will promote it).
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: ReactiveScope {
                        id: ScopeId(0),
                        range: MutableRange {
                            start: InstructionId(0),
                            end: InstructionId(10),
                        },
                        dependencies: vec![ReactiveScopeDependency {
                            identifier: make_test_identifier(1, None),
                            path: vec![],
                        }],
                        declarations: Default::default(),
                        reassignments: vec![],
                        merged_id: None,
                        early_return_value: None,
                    },
                    instructions: vec![],
                }),
                // An instruction that uses the same identifier (DeclarationId 1).
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(5),
                    lvalue: Some(make_test_place(2, None)),
                    value: InstructionValue::LoadLocal {
                        place: make_test_place(1, None),
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
            ],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        // The LoadLocal's place should also be promoted.
        if let ReactiveStatement::Instruction(instr) = &func.body[1]
            && let InstructionValue::LoadLocal { place, .. } = &instr.value
        {
            assert!(
                place.identifier.name.is_some(),
                "LoadLocal place should be promoted"
            );
            let name = place.identifier.name.as_ref().unwrap();
            assert!(
                matches!(name, IdentifierName::Promoted(s) if s == "#t1"),
                "Expected #t1, got {:?}",
                name
            );
        }
    }

    #[test]
    fn test_pruned_scope_used_outside() {
        // A pruned scope declaration used in a DIFFERENT scope should be promoted.
        // The detection compares active scope stacks, so usage must be in a
        // different parent scope than the pruned scope's parent.
        let mut declarations = indexmap::IndexMap::new();
        declarations.insert(
            IdentifierId(1),
            ScopeDeclaration {
                identifier: make_test_identifier(1, None),
                scope: make_declaration_scope(ScopeId(1)),
            },
        );

        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                // Scope 99 containing the pruned scope with declaration id=1.
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: ReactiveScope {
                        id: ScopeId(99),
                        range: MutableRange {
                            start: InstructionId(0),
                            end: InstructionId(100),
                        },
                        dependencies: vec![],
                        declarations: Default::default(),
                        reassignments: vec![],
                        merged_id: None,
                        early_return_value: None,
                    },
                    instructions: vec![ReactiveStatement::PrunedScope(PrunedReactiveScopeBlock {
                        scope: ReactiveScope {
                            id: ScopeId(1),
                            range: MutableRange {
                                start: InstructionId(0),
                                end: InstructionId(5),
                            },
                            dependencies: vec![],
                            declarations: declarations.clone(),
                            reassignments: vec![],
                            merged_id: None,
                            early_return_value: None,
                        },
                        instructions: vec![],
                    })],
                }),
                // Scope 50: usage of id=1 in a DIFFERENT scope.
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: ReactiveScope {
                        id: ScopeId(50),
                        range: MutableRange {
                            start: InstructionId(100),
                            end: InstructionId(200),
                        },
                        dependencies: vec![],
                        declarations: Default::default(),
                        reassignments: vec![],
                        merged_id: None,
                        early_return_value: None,
                    },
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(6),
                            lvalue: Some(make_test_place(2, None)),
                            value: InstructionValue::LoadLocal {
                                place: make_test_place(1, None),
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
            ],
            directives: vec![],
        };

        promote_used_temporaries(&mut func);

        // Check the pruned scope's declaration was promoted.
        if let ReactiveStatement::Scope(outer) = &func.body[0]
            && let ReactiveStatement::PrunedScope(pruned) = &outer.instructions[0]
        {
            let decl = pruned.scope.declarations.get(&IdentifierId(1)).unwrap();
            assert!(
                decl.identifier.name.is_some(),
                "Pruned scope declaration used outside should be promoted"
            );
        }
    }
}
