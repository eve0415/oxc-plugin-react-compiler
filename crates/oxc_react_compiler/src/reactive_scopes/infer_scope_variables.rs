//! Infer reactive scope variables — port of InferReactiveScopeVariables.ts.
//!
//! This is the 1st of 4 passes that determine how to break a function into
//! discrete reactive scopes (independently memoizeable units of code):
//! 1. InferReactiveScopeVariables (this pass, on HIR) determines operands that
//!    mutate together and assigns them a unique reactive scope.
//! 2. AlignReactiveScopesToBlockScopes (on ReactiveFunction) aligns reactive
//!    scopes to block scopes.
//! 3. MergeOverlappingReactiveScopes (on ReactiveFunction) ensures that reactive
//!    scopes do not overlap, merging any such scopes.
//! 4. BuildReactiveBlocks (on ReactiveFunction) groups the statements for each
//!    scope into a ReactiveScopeBlock.
//!
//! For each mutable variable, infers a reactive scope which will construct that
//! variable. Variables that co-mutate are assigned to the same reactive scope.
//! This pass does *not* infer the set of instructions necessary to compute each
//! variable/scope, only the set of variables that will be computed by each scope.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{BTreeMap, HashMap, HashSet};

use indexmap::IndexMap;

use crate::hir::types::*;
use crate::hir::visitors;

// ---------------------------------------------------------------------------
// Instruction numbering
// ---------------------------------------------------------------------------

/// Assign sequential InstructionIds to all instructions in the function.
/// This establishes the ordering needed for mutable range tracking.
pub fn number_instructions(func: &mut HIRFunction) -> InstructionId {
    let mut next_id = InstructionId::new(1); // Start at 1, 0 means "unset"
    for (_bid, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            instr.id = next_id;
            next_id = InstructionId::new(next_id.0 + 1);
        }
        set_terminal_id(&mut block.terminal, next_id);
        next_id = InstructionId::new(next_id.0 + 1);
    }
    next_id
}

fn set_terminal_id(terminal: &mut Terminal, id: InstructionId) {
    match terminal {
        Terminal::Return { id: tid, .. }
        | Terminal::Throw { id: tid, .. }
        | Terminal::If { id: tid, .. }
        | Terminal::Branch { id: tid, .. }
        | Terminal::Goto { id: tid, .. }
        | Terminal::Switch { id: tid, .. }
        | Terminal::Try { id: tid, .. }
        | Terminal::Unsupported { id: tid, .. }
        | Terminal::Unreachable { id: tid, .. }
        | Terminal::For { id: tid, .. }
        | Terminal::ForOf { id: tid, .. }
        | Terminal::ForIn { id: tid, .. }
        | Terminal::While { id: tid, .. }
        | Terminal::DoWhile { id: tid, .. }
        | Terminal::Label { id: tid, .. }
        | Terminal::Scope { id: tid, .. }
        | Terminal::PrunedScope { id: tid, .. }
        | Terminal::Sequence { id: tid, .. }
        | Terminal::Logical { id: tid, .. }
        | Terminal::Ternary { id: tid, .. }
        | Terminal::Optional { id: tid, .. }
        | Terminal::MaybeThrow { id: tid, .. } => {
            *tid = id;
        }
    }
}

// ---------------------------------------------------------------------------
// Union-Find (DisjointSet)
// ---------------------------------------------------------------------------

struct DisjointSet {
    parent: HashMap<IdentifierId, IdentifierId>,
    rank: HashMap<IdentifierId, u32>,
}

impl DisjointSet {
    fn new() -> Self {
        Self {
            parent: HashMap::new(),
            rank: HashMap::new(),
        }
    }

    fn make_set(&mut self, id: IdentifierId) {
        if let std::collections::hash_map::Entry::Vacant(e) = self.parent.entry(id) {
            e.insert(id);
            self.rank.insert(id, 0);
        }
    }

    fn find(&mut self, id: IdentifierId) -> IdentifierId {
        let p = *self.parent.get(&id).unwrap_or(&id);
        if p != id {
            let root = self.find(p);
            self.parent.insert(id, root);
            root
        } else {
            id
        }
    }

    fn union(&mut self, a: IdentifierId, b: IdentifierId) {
        self.make_set(a);
        self.make_set(b);
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        let rank_a = *self.rank.get(&ra).unwrap_or(&0);
        let rank_b = *self.rank.get(&rb).unwrap_or(&0);
        if rank_a < rank_b {
            self.parent.insert(ra, rb);
        } else if rank_a > rank_b {
            self.parent.insert(rb, ra);
        } else {
            self.parent.insert(rb, ra);
            self.rank.insert(ra, rank_a + 1);
        }
    }

    /// Union a list of identifiers together.
    fn union_all(&mut self, ids: &[IdentifierId]) {
        if ids.len() < 2 {
            return;
        }
        for i in 1..ids.len() {
            self.union(ids[0], ids[i]);
        }
    }
}

// ---------------------------------------------------------------------------
// Mutable range inference (simplified — for non-aliasing pipeline)
// ---------------------------------------------------------------------------

/// Infer mutable ranges for all identifiers.
/// Used only when aliasing ranges are NOT pre-computed.
pub fn infer_mutable_ranges(func: &mut HIRFunction) {
    let mut first_seen: HashMap<IdentifierId, InstructionId> = HashMap::new();
    let mut last_mutated: HashMap<IdentifierId, InstructionId> = HashMap::new();
    let mut load_source: HashMap<IdentifierId, IdentifierId> = HashMap::new();

    let id_to_name = build_name_lookup(func);
    let mut frozen_ids: std::collections::HashSet<IdentifierId> = std::collections::HashSet::new();

    // Pass 1: Collect creation points and mutation points
    for (_bid, block) in &func.body.blocks {
        let phi_id = block
            .instructions
            .first()
            .map(|i| i.id)
            .unwrap_or(InstructionId::new(1));
        for phi in &block.phis {
            record_first(&mut first_seen, phi.place.identifier.id, phi_id);
            for operand in phi.operands.values() {
                record_first(&mut first_seen, operand.identifier.id, phi_id);
            }
            let any_operand_mutated = phi
                .operands
                .values()
                .any(|op| last_mutated.contains_key(&op.identifier.id));
            if any_operand_mutated {
                record_mutation(&mut last_mutated, phi.place.identifier.id, phi_id);
            }
        }

        for instr in &block.instructions {
            let id = instr.id;
            record_first(&mut first_seen, instr.lvalue.identifier.id, id);

            if may_allocate(&instr.value, &instr.lvalue.identifier.type_) {
                record_mutation(&mut last_mutated, instr.lvalue.identifier.id, id);
            }

            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    record_first(&mut first_seen, lvalue.place.identifier.id, id);
                    record_first(&mut first_seen, value.identifier.id, id);
                    // Upstream InferMutationAliasingRanges keeps assignment values
                    // live through the store instruction. Without this, temps
                    // introduced by sequence/value blocks can collapse to a
                    // one-instruction range and get dropped from scope unions.
                    record_mutation(&mut last_mutated, value.identifier.id, id);
                    if lvalue.kind == InstructionKind::Reassign
                        && last_mutated.contains_key(&value.identifier.id)
                    {
                        record_mutation(&mut last_mutated, lvalue.place.identifier.id, id);
                        record_mutation(&mut last_mutated, value.identifier.id, id);
                    }
                }
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    record_first(&mut first_seen, lvalue.place.identifier.id, id);
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    record_first(&mut first_seen, value.identifier.id, id);
                    for_each_pattern_id(&lvalue.pattern, &mut |pid| {
                        record_first(&mut first_seen, pid, id);
                    });
                }
                InstructionValue::PropertyStore { object, .. }
                | InstructionValue::PropertyDelete { object, .. } => {
                    record_mutation(&mut last_mutated, object.identifier.id, id);
                    if let Some(&source_id) = load_source.get(&object.identifier.id)
                        && last_mutated.contains_key(&source_id)
                    {
                        record_mutation(&mut last_mutated, source_id, id);
                    }
                }
                InstructionValue::ComputedStore { object, .. }
                | InstructionValue::ComputedDelete { object, .. } => {
                    record_mutation(&mut last_mutated, object.identifier.id, id);
                    if let Some(&source_id) = load_source.get(&object.identifier.id)
                        && last_mutated.contains_key(&source_id)
                    {
                        record_mutation(&mut last_mutated, source_id, id);
                    }
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    record_mutation(&mut last_mutated, lvalue.identifier.id, id);
                }
                InstructionValue::CallExpression { args, callee, .. } => {
                    let callee_is_hook =
                        is_hook_callee(&callee.identifier, &id_to_name, &load_source);

                    if !callee_is_hook {
                        for arg in args {
                            match arg {
                                Argument::Place(p) | Argument::Spread(p) => {
                                    if frozen_ids.contains(&p.identifier.id) {
                                        continue;
                                    }
                                    if last_mutated.contains_key(&p.identifier.id) {
                                        record_mutation(&mut last_mutated, p.identifier.id, id);
                                    }
                                    if let Some(&source_id) = load_source.get(&p.identifier.id) {
                                        if frozen_ids.contains(&source_id) {
                                            continue;
                                        }
                                        if last_mutated.contains_key(&source_id) {
                                            record_mutation(&mut last_mutated, source_id, id);
                                            record_mutation(&mut last_mutated, p.identifier.id, id);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        for arg in args {
                            match arg {
                                Argument::Place(p) | Argument::Spread(p) => {
                                    record_first(&mut first_seen, p.identifier.id, id);
                                    frozen_ids.insert(p.identifier.id);
                                    if let Some(&source_id) = load_source.get(&p.identifier.id) {
                                        frozen_ids.insert(source_id);
                                    }
                                }
                            }
                        }
                    }
                    if last_mutated.contains_key(&callee.identifier.id) {
                        record_mutation(&mut last_mutated, callee.identifier.id, id);
                    }
                }
                InstructionValue::NewExpression { args, .. } => {
                    for arg in args {
                        match arg {
                            Argument::Place(p) | Argument::Spread(p) => {
                                if last_mutated.contains_key(&p.identifier.id) {
                                    record_mutation(&mut last_mutated, p.identifier.id, id);
                                }
                                if let Some(&source_id) = load_source.get(&p.identifier.id)
                                    && last_mutated.contains_key(&source_id)
                                {
                                    record_mutation(&mut last_mutated, source_id, id);
                                    record_mutation(&mut last_mutated, p.identifier.id, id);
                                }
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
                    let callee_is_hook =
                        is_hook_callee(&property.identifier, &id_to_name, &load_source);
                    if callee_is_hook {
                        for arg in args {
                            match arg {
                                Argument::Place(p) | Argument::Spread(p) => {
                                    record_first(&mut first_seen, p.identifier.id, id);
                                    frozen_ids.insert(p.identifier.id);
                                    if let Some(&source_id) = load_source.get(&p.identifier.id) {
                                        frozen_ids.insert(source_id);
                                    }
                                }
                            }
                        }
                    } else {
                        let mutates_receiver =
                            method_call_mutates_receiver(&instr.value, &id_to_name, &load_source);
                        if mutates_receiver {
                            if last_mutated.contains_key(&receiver.identifier.id) {
                                record_mutation(&mut last_mutated, receiver.identifier.id, id);
                            }
                            if let Some(&source_id) = load_source.get(&receiver.identifier.id)
                                && last_mutated.contains_key(&source_id)
                            {
                                record_mutation(&mut last_mutated, source_id, id);
                                record_mutation(&mut last_mutated, receiver.identifier.id, id);
                            }
                        }
                        for arg in args {
                            match arg {
                                Argument::Place(p) | Argument::Spread(p) => {
                                    record_first(&mut first_seen, p.identifier.id, id);
                                }
                            }
                        }
                    }
                }
                InstructionValue::Ternary {
                    test,
                    consequent,
                    alternate,
                    ..
                } => {
                    record_first(&mut first_seen, test.identifier.id, id);
                    record_first(&mut first_seen, consequent.identifier.id, id);
                    record_first(&mut first_seen, alternate.identifier.id, id);

                    let mut branch_mutable = false;
                    for branch in [consequent, alternate] {
                        if last_mutated.contains_key(&branch.identifier.id) {
                            record_mutation(&mut last_mutated, branch.identifier.id, id);
                            branch_mutable = true;
                        }
                        if let Some(&source_id) = load_source.get(&branch.identifier.id)
                            && last_mutated.contains_key(&source_id)
                        {
                            record_mutation(&mut last_mutated, source_id, id);
                            record_mutation(&mut last_mutated, branch.identifier.id, id);
                            branch_mutable = true;
                        }
                    }
                    if branch_mutable {
                        record_mutation(&mut last_mutated, instr.lvalue.identifier.id, id);
                    }
                }
                InstructionValue::LoadLocal { place, .. } => {
                    load_source.insert(instr.lvalue.identifier.id, place.identifier.id);
                    record_first(&mut first_seen, place.identifier.id, id);
                }
                _ => {
                    visitors::for_each_instruction_operand(instr, |place| {
                        record_first(&mut first_seen, place.identifier.id, id);
                    });
                }
            }
        }
    }

    // Pass 2: Apply mutable ranges to identifiers
    for (_bid, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            apply_range(&first_seen, &last_mutated, &mut phi.place.identifier);
            for operand in phi.operands.values_mut() {
                apply_range(&first_seen, &last_mutated, &mut operand.identifier);
            }
        }
        for instr in &mut block.instructions {
            apply_range(&first_seen, &last_mutated, &mut instr.lvalue.identifier);
            visitors::map_instruction_operands(instr, |place| {
                apply_range(&first_seen, &last_mutated, &mut place.identifier);
            });
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    apply_range(&first_seen, &last_mutated, &mut lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    map_pattern_identifiers(&mut lvalue.pattern, &mut |ident| {
                        apply_range(&first_seen, &last_mutated, ident);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    apply_range(&first_seen, &last_mutated, &mut lvalue.identifier);
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mutable range helpers
// ---------------------------------------------------------------------------

fn build_name_lookup(func: &HIRFunction) -> HashMap<IdentifierId, String> {
    let mut id_to_name: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(IdentifierName::Named(name)) = &instr.lvalue.identifier.name {
                id_to_name.insert(instr.lvalue.identifier.id, name.clone());
            }
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(IdentifierName::Named(name)) = &place.identifier.name {
                        id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                    }
                    if !id_to_name.contains_key(&instr.lvalue.identifier.id)
                        && let Some(name) = id_to_name.get(&place.identifier.id)
                    {
                        let name = name.clone();
                        id_to_name.insert(instr.lvalue.identifier.id, name);
                    }
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    id_to_name.insert(
                        instr.lvalue.identifier.id,
                        load_global_name_for_hook_detection(binding),
                    );
                }
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(name),
                    ..
                } => {
                    id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                }
                InstructionValue::Primitive { .. } => {}
                InstructionValue::PropertyLoad {
                    property: PropertyLiteral::String(name),
                    ..
                } => {
                    id_to_name.insert(instr.lvalue.identifier.id, name.clone());
                }
                InstructionValue::ComputedLoad { property, .. } => {
                    if let Some(mapped) = id_to_name.get(&property.identifier.id) {
                        id_to_name.insert(instr.lvalue.identifier.id, mapped.clone());
                    }
                }
                _ => {}
            }
        }
    }
    id_to_name
}

fn is_hook_callee(
    callee_ident: &Identifier,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    if let Type::Function {
        shape_id: Some(shape_id),
        ..
    } = &callee_ident.type_
        && matches!(
            shape_id.as_str(),
            "BuiltInUseEffectHookId" | "BuiltInUseImperativeHandleHookId"
        )
    {
        return false;
    }

    if is_hook_function_type(&callee_ident.type_) {
        return true;
    }

    let is_effect_name = |name: &str| -> bool {
        let candidate = normalize_hook_name(name);
        matches!(
            candidate,
            "useEffect" | "useLayoutEffect" | "useInsertionEffect" | "useImperativeHandle"
        )
    };

    if let Some(IdentifierName::Named(name)) = &callee_ident.name {
        if is_effect_name(name) {
            return false;
        }
        if is_hook_name_str(name) {
            return true;
        }
    }
    if let Some(name) = id_to_name.get(&callee_ident.id) {
        if is_effect_name(name) {
            return false;
        }
        if is_hook_name_str(name) {
            return true;
        }
    }
    if let Some(&source_id) = load_source.get(&callee_ident.id)
        && let Some(name) = id_to_name.get(&source_id)
    {
        if is_effect_name(name) {
            return false;
        }
        if is_hook_name_str(name) {
            return true;
        }
    }
    false
}

fn is_hook_function_type(ty: &Type) -> bool {
    match ty {
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } => matches!(
            shape_id.as_str(),
            "BuiltInUseStateHookId"
                | "BuiltInUseReducerHookId"
                | "BuiltInUseContextHookId"
                | "BuiltInUseRefHookId"
                | "BuiltInUseMemoHookId"
                | "BuiltInUseCallbackHookId"
                | "BuiltInUseEffectHookId"
                | "BuiltInUseTransitionHookId"
                | "BuiltInUseImperativeHandleHookId"
                | "BuiltInUseActionStateHookId"
                | "BuiltInDefaultMutatingHookId"
                | "BuiltInDefaultNonmutatingHookId"
        ),
        _ => false,
    }
}

fn is_hook_name_str(name: &str) -> bool {
    let candidate = normalize_hook_name(name);
    if let Some(rest) = candidate.strip_prefix("use") {
        rest.is_empty() || rest.chars().next().is_some_and(|c| c.is_uppercase())
    } else {
        false
    }
}

fn normalize_hook_name(name: &str) -> &str {
    let tail = name.rsplit_once('.').map_or(name, |(_, tail)| tail);
    tail.rsplit_once('$').map_or(tail, |(_, tail)| tail)
}

fn load_global_name_for_hook_detection(binding: &NonLocalBinding) -> String {
    match binding {
        NonLocalBinding::ImportSpecifier { imported, .. } => imported.clone(),
        _ => binding.name().to_string(),
    }
}

fn method_call_mutates_receiver(
    value: &InstructionValue,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    let property_ident = match value {
        InstructionValue::MethodCall { property, .. } => &property.identifier,
        _ => return true,
    };
    let method_name = resolve_identifier_name(property_ident, id_to_name, load_source);
    match method_name.as_deref() {
        Some(name) => !is_known_non_mutating_method_name(name),
        None => true,
    }
}

fn resolve_identifier_name(
    ident: &Identifier,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) -> Option<String> {
    match &ident.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            return Some(name.clone());
        }
        None => {}
    }
    if let Some(name) = id_to_name.get(&ident.id) {
        return Some(name.clone());
    }
    if let Some(source_id) = load_source.get(&ident.id)
        && let Some(name) = id_to_name.get(source_id)
    {
        return Some(name.clone());
    }
    None
}

fn is_known_non_mutating_method_name(name: &str) -> bool {
    matches!(
        name,
        "at" | "concat"
            | "entries"
            | "every"
            | "filter"
            | "find"
            | "findIndex"
            | "findLast"
            | "findLastIndex"
            | "flat"
            | "flatMap"
            | "forEach"
            | "get"
            | "has"
            | "includes"
            | "indexOf"
            | "join"
            | "keys"
            | "lastIndexOf"
            | "map"
            | "reduce"
            | "reduceRight"
            | "slice"
            | "some"
            | "toLocaleString"
            | "toReversed"
            | "toSorted"
            | "toSpliced"
            | "toString"
            | "values"
            | "with"
    )
}

fn record_first(
    map: &mut HashMap<IdentifierId, InstructionId>,
    id: IdentifierId,
    instr_id: InstructionId,
) {
    map.entry(id).or_insert(instr_id);
}

fn record_mutation(
    map: &mut HashMap<IdentifierId, InstructionId>,
    id: IdentifierId,
    instr_id: InstructionId,
) {
    let entry = map.entry(id).or_insert(instr_id);
    if instr_id.0 > entry.0 {
        *entry = instr_id;
    }
}

fn apply_range(
    first_seen: &HashMap<IdentifierId, InstructionId>,
    last_mutated: &HashMap<IdentifierId, InstructionId>,
    ident: &mut Identifier,
) {
    let start = first_seen
        .get(&ident.id)
        .copied()
        .unwrap_or(InstructionId::new(0));
    let end = last_mutated
        .get(&ident.id)
        .copied()
        .map(|e| InstructionId::new(e.0 + 1))
        .unwrap_or(InstructionId::new(start.0 + 1));
    ident.mutable_range = MutableRange { start, end };
}

fn for_each_pattern_id(pattern: &Pattern, f: &mut impl FnMut(IdentifierId)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        f(place.identifier.id);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(p.place.identifier.id);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        f(place.identifier.id);
                    }
                }
            }
        }
    }
}

fn map_pattern_identifiers(pattern: &mut Pattern, f: &mut impl FnMut(&mut Identifier)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        f(&mut place.identifier);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        f(&mut p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        f(&mut place.identifier);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// mayAllocate — port of upstream mayAllocate()
// ---------------------------------------------------------------------------

/// Check if an instruction may allocate a new value.
///
/// Exact port of upstream `mayAllocate()`. Determines whether an instruction
/// creates a new mutable value that needs a reactive scope.
fn may_allocate(value: &InstructionValue, lvalue_type: &Type) -> bool {
    match value {
        // Destructure: only allocates if the pattern contains a spread element.
        InstructionValue::Destructure { lvalue, .. } => pattern_contains_spread(&lvalue.pattern),

        // These never allocate:
        InstructionValue::PostfixUpdate { .. }
        | InstructionValue::PrefixUpdate { .. }
        | InstructionValue::Await { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StoreLocal { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::TypeCastExpression { .. }
        | InstructionValue::LoadLocal { .. }
        | InstructionValue::LoadContext { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::ComputedLoad { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::GetIterator { .. }
        | InstructionValue::IteratorNext { .. }
        | InstructionValue::NextPropertyOf { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::UnaryExpression { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::PropertyLoad { .. }
        | InstructionValue::StoreGlobal { .. } => false,

        // Call-like: allocate only if return type is non-primitive.
        InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::CallExpression { .. }
        | InstructionValue::MethodCall { .. } => !matches!(lvalue_type, Type::Primitive),

        // These always allocate:
        InstructionValue::RegExpLiteral { .. }
        | InstructionValue::PropertyStore { .. }
        | InstructionValue::ComputedStore { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::JsxExpression { .. }
        | InstructionValue::JsxFragment { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::ObjectExpression { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::FunctionExpression { .. } => true,

        // Rust-only instruction kinds not in upstream — conservative default.
        InstructionValue::Ternary { .. }
        | InstructionValue::LogicalExpression { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. } => false,
    }
}

/// Check if a destructuring pattern contains any spread elements.
fn pattern_contains_spread(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Array(arr) => arr
            .items
            .iter()
            .any(|item| matches!(item, ArrayElement::Spread(_))),
        Pattern::Object(obj) => obj
            .properties
            .iter()
            .any(|p| matches!(p, ObjectPropertyOrSpread::Spread(_))),
    }
}

// ---------------------------------------------------------------------------
// isMutable / inRange — port of upstream isMutable/inRange
// ---------------------------------------------------------------------------

/// Is the operand mutable at this given instruction?
fn is_mutable(instr_id: InstructionId, range: &MutableRange) -> bool {
    instr_id.0 >= range.start.0 && instr_id.0 < range.end.0
}

/// Optional member accesses and calls are lowered as single instructions in Rust
/// HIR, unlike upstream's branch-based lowering. To preserve co-mutation
/// behavior for values produced immediately before an optional access, also
/// treat `range.end == instr_id` as mutable at that instruction.
fn is_mutable_for_optional_access(instr_id: InstructionId, range: &MutableRange) -> bool {
    is_mutable(instr_id, range) || range.end.0 == instr_id.0
}

// ---------------------------------------------------------------------------
// findDisjointMutableValues — exact port of upstream
// ---------------------------------------------------------------------------

/// Find groups of identifiers that must be in the same reactive scope.
///
/// Exact port of upstream `findDisjointMutableValues()`.
/// All mutable operands (including lvalue) of an instruction are grouped
/// into the same scope via union-find.
fn find_disjoint_mutable_values(func: &HIRFunction) -> DisjointSet {
    let mut scope_identifiers = DisjointSet::new();
    let id_to_name = build_name_lookup(func);
    let enable_treat_function_deps_as_conditional =
        func.env.config().enable_treat_function_deps_as_conditional;
    let mut optional_call_arg_decls: HashSet<DeclarationId> = HashSet::new();
    let mut call_like_result_ids: HashSet<IdentifierId> = HashSet::new();
    let mut conditional_function_call_result_ids: HashSet<IdentifierId> = HashSet::new();
    let mut conditional_function_call_operand_ids: HashMap<IdentifierId, Vec<IdentifierId>> =
        HashMap::new();
    let mut load_global_ids: HashSet<IdentifierId> = HashSet::new();
    let mut store_result_target_decl: HashMap<IdentifierId, DeclarationId> = HashMap::new();
    let mut load_result_source_decl: HashMap<IdentifierId, DeclarationId> = HashMap::new();
    let mut value_block_store_blocks: HashMap<DeclarationId, HashSet<BlockId>> = HashMap::new();
    let mut value_block_store_targets: HashMap<DeclarationId, Vec<IdentifierId>> = HashMap::new();
    let mut value_block_store_sources: HashMap<DeclarationId, Vec<IdentifierId>> = HashMap::new();

    // Collect direct optional-call argument declarations.
    for (bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if matches!(
                instr.value,
                InstructionValue::CallExpression { .. }
                    | InstructionValue::MethodCall { .. }
                    | InstructionValue::TaggedTemplateExpression { .. }
            ) {
                call_like_result_ids.insert(instr.lvalue.identifier.id);
            }
            if let InstructionValue::CallExpression { callee, .. } = &instr.value
                && callee.identifier.mutable_range.start.0 > 0
            {
                conditional_function_call_result_ids.insert(instr.lvalue.identifier.id);
                let mut operand_ids = Vec::new();
                visitors::for_each_instruction_operand(instr, |place| {
                    if place.identifier.mutable_range.start.0 > 0 {
                        operand_ids.push(place.identifier.id);
                    }
                });
                if !operand_ids.is_empty() {
                    conditional_function_call_operand_ids
                        .insert(instr.lvalue.identifier.id, operand_ids);
                }
            }
            if matches!(instr.value, InstructionValue::LoadGlobal { .. }) {
                load_global_ids.insert(instr.lvalue.identifier.id);
            }
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    store_result_target_decl.insert(
                        instr.lvalue.identifier.id,
                        lvalue.place.identifier.declaration_id,
                    );
                    if block.kind == BlockKind::Value {
                        let value = match &instr.value {
                            InstructionValue::StoreLocal { value, .. }
                            | InstructionValue::StoreContext { value, .. } => value,
                            _ => unreachable!(),
                        };
                        value_block_store_blocks
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_default()
                            .insert(*bid);
                        value_block_store_targets
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_default()
                            .push(lvalue.place.identifier.id);
                        value_block_store_sources
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_default()
                            .push(value.identifier.id);
                    }
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    load_result_source_decl
                        .insert(instr.lvalue.identifier.id, place.identifier.declaration_id);
                }
                _ => {}
            }
            match &instr.value {
                InstructionValue::CallExpression { args, optional, .. } if *optional => {
                    for arg in args {
                        match arg {
                            Argument::Place(p) | Argument::Spread(p) => {
                                optional_call_arg_decls.insert(p.identifier.declaration_id);
                            }
                        }
                    }
                }
                InstructionValue::MethodCall {
                    args,
                    receiver_optional,
                    call_optional,
                    ..
                } if *receiver_optional || *call_optional => {
                    for arg in args {
                        match arg {
                            Argument::Place(p) | Argument::Spread(p) => {
                                optional_call_arg_decls.insert(p.identifier.declaration_id);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut value_block_result_direct_return_counts: HashMap<DeclarationId, usize> = HashMap::new();
    let mut value_block_result_outside_use_counts: HashMap<DeclarationId, usize> = HashMap::new();
    if enable_treat_function_deps_as_conditional && !value_block_store_blocks.is_empty() {
        let tracked_value_block_decls: HashSet<DeclarationId> =
            value_block_store_blocks.keys().copied().collect();
        for (bid, block) in &func.body.blocks {
            let is_store_block_for_decl = |decl_id: DeclarationId| {
                value_block_store_blocks
                    .get(&decl_id)
                    .is_some_and(|store_blocks| store_blocks.contains(bid))
            };

            visitors::for_each_terminal_operand(&block.terminal, |place| {
                let decl_id = place.identifier.declaration_id;
                if !tracked_value_block_decls.contains(&decl_id) || is_store_block_for_decl(decl_id)
                {
                    return;
                }
                if matches!(&block.terminal, Terminal::Return { .. }) {
                    *value_block_result_direct_return_counts
                        .entry(decl_id)
                        .or_default() += 1;
                } else {
                    *value_block_result_outside_use_counts
                        .entry(decl_id)
                        .or_default() += 1;
                }
            });

            for instr in &block.instructions {
                visitors::for_each_instruction_operand(instr, |place| {
                    let decl_id = place.identifier.declaration_id;
                    if !tracked_value_block_decls.contains(&decl_id)
                        || is_store_block_for_decl(decl_id)
                    {
                        return;
                    }
                    *value_block_result_outside_use_counts
                        .entry(decl_id)
                        .or_default() += 1;
                });
            }
        }
    }

    // Track declarations: DeclarationId -> IdentifierId
    let mut declarations: HashMap<DeclarationId, IdentifierId> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        // Phi handling: if a phi is mutated after creation, alias all its operands
        // together into the same scope.
        //
        // Upstream condition:
        //   phi.place.identifier.mutableRange.start + 1 !== phi.place.identifier.mutableRange.end
        //   && phi.place.identifier.mutableRange.end > (block.instructions.at(0)?.id ?? block.terminal.id)
        let first_instr_id = block
            .instructions
            .first()
            .map(|i| i.id)
            .unwrap_or_else(|| get_terminal_id(&block.terminal));

        for phi in &block.phis {
            let range = &phi.place.identifier.mutable_range;
            if range.start.0 + 1 != range.end.0 && range.end.0 > first_instr_id.0 {
                let mut operands = vec![phi.place.identifier.id];

                // Look up the declaration for this phi
                if let Some(&decl_id) = declarations.get(&phi.place.identifier.declaration_id) {
                    operands.push(decl_id);
                }

                for op in phi.operands.values() {
                    operands.push(op.identifier.id);
                }

                // Make sets for all before union
                for &id in &operands {
                    scope_identifiers.make_set(id);
                }
                scope_identifiers.union_all(&operands);
            }
            // Note: enableForest branch omitted (not enabled in our config)
        }

        for (instr_index, instr) in block.instructions.iter().enumerate() {
            let mut operands: Vec<IdentifierId> = Vec::new();

            // Include lvalue if its mutable range is wide or the instruction allocates
            let lv_range = &instr.lvalue.identifier.mutable_range;
            if lv_range.end.0 > lv_range.start.0 + 1
                || may_allocate(&instr.value, &instr.lvalue.identifier.type_)
            {
                scope_identifiers.make_set(instr.lvalue.identifier.id);
                operands.push(instr.lvalue.identifier.id);
            }

            match &instr.value {
                // DeclareLocal / DeclareContext: just register declaration
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    declarations
                        .entry(lvalue.place.identifier.declaration_id)
                        .or_insert(lvalue.place.identifier.id);
                }

                // StoreLocal / StoreContext
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    declarations
                        .entry(lvalue.place.identifier.declaration_id)
                        .or_insert(lvalue.place.identifier.id);

                    // Include target if wide mutable range
                    if lvalue.place.identifier.mutable_range.end.0
                        > lvalue.place.identifier.mutable_range.start.0 + 1
                    {
                        scope_identifiers.make_set(lvalue.place.identifier.id);
                        operands.push(lvalue.place.identifier.id);
                    }

                    // Include value if mutable at this instruction and not global
                    if is_mutable(instr.id, &value.identifier.mutable_range)
                        && value.identifier.mutable_range.start.0 > 0
                    {
                        scope_identifiers.make_set(value.identifier.id);
                        operands.push(value.identifier.id);
                    }
                }

                // Destructure
                InstructionValue::Destructure { lvalue, value, .. } => {
                    visitors::for_each_pattern_place(&lvalue.pattern, &mut |place| {
                        declarations
                            .entry(place.identifier.declaration_id)
                            .or_insert(place.identifier.id);

                        if place.identifier.mutable_range.end.0
                            > place.identifier.mutable_range.start.0 + 1
                        {
                            scope_identifiers.make_set(place.identifier.id);
                            operands.push(place.identifier.id);
                        }
                    });

                    if is_mutable(instr.id, &value.identifier.mutable_range)
                        && value.identifier.mutable_range.start.0 > 0
                    {
                        scope_identifiers.make_set(value.identifier.id);
                        operands.push(value.identifier.id);
                    }
                }

                // If this JSX value is itself used as an optional-call argument,
                // keep its immediate expression operands in the same scope.
                InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
                    if optional_call_arg_decls
                        .contains(&instr.lvalue.identifier.declaration_id) =>
                {
                    visitors::for_each_instruction_operand(instr, |place| {
                        if is_mutable_for_optional_access(instr.id, &place.identifier.mutable_range)
                            && place.identifier.mutable_range.start.0 > 0
                        {
                            scope_identifiers.make_set(place.identifier.id);
                            operands.push(place.identifier.id);
                        }
                    });
                }

                // Optional CallExpression: include "just-produced" mutable operands
                // so optional call args can co-mutate with the call result.
                InstructionValue::CallExpression { optional: true, .. } => {
                    visitors::for_each_instruction_operand(instr, |place| {
                        if is_mutable_for_optional_access(instr.id, &place.identifier.mutable_range)
                            && place.identifier.mutable_range.start.0 > 0
                        {
                            scope_identifiers.make_set(place.identifier.id);
                            operands.push(place.identifier.id);
                        }
                    });
                }

                // Rust lowers optional member expressions as single instructions,
                // so also keep immediately preceding mutable operands in the same
                // scope when resolving the optional result.
                InstructionValue::PropertyLoad { optional: true, .. }
                | InstructionValue::ComputedLoad { optional: true, .. } => {
                    scope_identifiers.make_set(instr.lvalue.identifier.id);
                    operands.push(instr.lvalue.identifier.id);
                    visitors::for_each_instruction_operand(instr, |place| {
                        if is_mutable_for_optional_access(instr.id, &place.identifier.mutable_range)
                            && place.identifier.mutable_range.start.0 > 0
                        {
                            scope_identifiers.make_set(place.identifier.id);
                            operands.push(place.identifier.id);
                        }
                    });
                }

                // MethodCall: iterate all operands for mutability, PLUS always
                // include the property identifier to keep ComputedLoad in same scope
                InstructionValue::MethodCall {
                    property,
                    receiver_optional,
                    call_optional,
                    ..
                } => {
                    let property_is_hook = match &property.identifier.name {
                        Some(IdentifierName::Named(name))
                        | Some(IdentifierName::Promoted(name)) => is_hook_name_str(name),
                        None => id_to_name
                            .get(&property.identifier.id)
                            .is_some_and(|name| is_hook_name_str(name)),
                    };
                    if !property_is_hook {
                        visitors::for_each_instruction_operand(instr, |place| {
                            let is_optional_call = *receiver_optional || *call_optional;
                            let mutable = if is_optional_call {
                                is_mutable_for_optional_access(
                                    instr.id,
                                    &place.identifier.mutable_range,
                                )
                            } else {
                                is_mutable(instr.id, &place.identifier.mutable_range)
                            };
                            if mutable && place.identifier.mutable_range.start.0 > 0 {
                                scope_identifiers.make_set(place.identifier.id);
                                operands.push(place.identifier.id);
                            }
                        });
                        // Always include property to keep method resolution in same scope.
                        scope_identifiers.make_set(property.identifier.id);
                        operands.push(property.identifier.id);
                    }
                }

                // Rust-only lowering can materialize assignment conditionals as
                // ternary merge instructions. Upstream lowers these through
                // explicit control-flow, which keeps branch temps in the same
                // reactive region. Detect only assignment-like ternaries here.
                InstructionValue::Ternary { .. } => {
                    // Narrow parity bridge for Rust-only lowered conditional merge
                    // points: only widen ternary unions for assignment-like branches
                    // (`cond ? (x = ...) : x`) which upstream lowers via control flow.
                    let (consequent, alternate) = match &instr.value {
                        InstructionValue::Ternary {
                            consequent,
                            alternate,
                            ..
                        } => (consequent, alternate),
                        _ => unreachable!(),
                    };
                    let consequent_target = store_result_target_decl
                        .get(&consequent.identifier.id)
                        .copied();
                    let alternate_target = store_result_target_decl
                        .get(&alternate.identifier.id)
                        .copied();
                    let consequent_source = load_result_source_decl
                        .get(&consequent.identifier.id)
                        .copied();
                    let alternate_source = load_result_source_decl
                        .get(&alternate.identifier.id)
                        .copied();
                    let assignment_like_ternary = consequent_target
                        .is_some_and(|decl| alternate_source.is_some_and(|source| source == decl))
                        || alternate_target.is_some_and(|decl| {
                            consequent_source.is_some_and(|source| source == decl)
                        });
                    let consequent_is_jsx = matches!(
                        consequent.identifier.type_,
                        Type::Object {
                            shape_id: Some(ref s)
                        } if s == "BuiltInJsx"
                    );
                    let alternate_is_jsx = matches!(
                        alternate.identifier.type_,
                        Type::Object {
                            shape_id: Some(ref s)
                        } if s == "BuiltInJsx"
                    );
                    let _consequent_is_primitive =
                        matches!(consequent.identifier.type_, Type::Primitive);
                    let _alternate_is_primitive =
                        matches!(alternate.identifier.type_, Type::Primitive);
                    // Keep JSX ternary co-mutation for expression-position ternaries
                    // (e.g. JSX children/attrs), but avoid assignment-position
                    // ternaries (`const x = cond ? jsx : null`) which upstream
                    // typically keeps as separate dependency scopes.
                    let has_jsx_branch = consequent_is_jsx || alternate_is_jsx;
                    let ternary_result_immediately_stored_local = block
                        .instructions
                        .get(instr_index + 1)
                        .is_some_and(|next| match &next.value {
                            InstructionValue::StoreLocal { value, .. }
                            | InstructionValue::StoreContext { value, .. } => {
                                value.identifier.id == instr.lvalue.identifier.id
                            }
                            _ => false,
                        });
                    let jsx_expression_ternary =
                        has_jsx_branch && !ternary_result_immediately_stored_local;

                    let mut ternary_operands: Vec<IdentifierId> = Vec::new();
                    visitors::for_each_instruction_operand(instr, |place| {
                        if place.identifier.mutable_range.start.0 > 0
                            && !load_global_ids.contains(&place.identifier.id)
                        {
                            ternary_operands.push(place.identifier.id);
                        }
                    });
                    let all_call_like_ternary = !ternary_operands.is_empty()
                        && ternary_operands
                            .iter()
                            .all(|id| call_like_result_ids.contains(id));

                    if all_call_like_ternary {
                        for id in ternary_operands {
                            scope_identifiers.make_set(id);
                            operands.push(id);
                        }
                        scope_identifiers.make_set(instr.lvalue.identifier.id);
                        operands.push(instr.lvalue.identifier.id);
                    } else if assignment_like_ternary || jsx_expression_ternary {
                        let mut merged_operand_found = false;
                        visitors::for_each_instruction_operand(instr, |place| {
                            let range = &place.identifier.mutable_range;
                            let near_merge_produced = range.end.0 == instr.id.0
                                || range.end.0.saturating_add(1) == instr.id.0;
                            if (is_mutable(instr.id, range) || near_merge_produced)
                                && place.identifier.mutable_range.start.0 > 0
                            {
                                scope_identifiers.make_set(place.identifier.id);
                                operands.push(place.identifier.id);
                                merged_operand_found = true;
                            }
                        });
                        if merged_operand_found {
                            visitors::for_each_instruction_operand(instr, |place| {
                                if place.identifier.mutable_range.start.0 > 0 {
                                    scope_identifiers.make_set(place.identifier.id);
                                    operands.push(place.identifier.id);
                                }
                            });
                            scope_identifiers.make_set(instr.lvalue.identifier.id);
                            operands.push(instr.lvalue.identifier.id);
                        }
                    } else if enable_treat_function_deps_as_conditional {
                        let mut merged_operand_found = false;
                        visitors::for_each_instruction_operand(instr, |place| {
                            let range = &place.identifier.mutable_range;
                            let near_merge_produced = range.end.0 == instr.id.0
                                || range.end.0.saturating_add(1) == instr.id.0;
                            if (is_mutable(instr.id, range) || near_merge_produced)
                                && place.identifier.mutable_range.start.0 > 0
                            {
                                scope_identifiers.make_set(place.identifier.id);
                                operands.push(place.identifier.id);
                                merged_operand_found = true;
                            }
                        });
                        if merged_operand_found {
                            scope_identifiers.make_set(instr.lvalue.identifier.id);
                            operands.push(instr.lvalue.identifier.id);
                        }
                    }
                }

                // Bridge for Rust's instruction-level logical lowering:
                // when the left operand is a call-like temporary, keep the
                // logical result in the same mutable set so downstream scope
                // building matches upstream value-block behavior.
                InstructionValue::LogicalExpression { left, right, .. } => {
                    if call_like_result_ids.contains(&left.identifier.id) {
                        if left.identifier.mutable_range.start.0 > 0
                            && !load_global_ids.contains(&left.identifier.id)
                        {
                            scope_identifiers.make_set(left.identifier.id);
                            operands.push(left.identifier.id);
                        }
                        if right.identifier.mutable_range.start.0 > 0
                            && !load_global_ids.contains(&right.identifier.id)
                        {
                            scope_identifiers.make_set(right.identifier.id);
                            operands.push(right.identifier.id);
                        }
                        scope_identifiers.make_set(instr.lvalue.identifier.id);
                        operands.push(instr.lvalue.identifier.id);
                    } else if enable_treat_function_deps_as_conditional {
                        let mut merged_operand_found = false;
                        visitors::for_each_instruction_operand(instr, |place| {
                            let range = &place.identifier.mutable_range;
                            let near_merge_produced = range.end.0 == instr.id.0
                                || range.end.0.saturating_add(1) == instr.id.0;
                            if (is_mutable(instr.id, range) || near_merge_produced)
                                && place.identifier.mutable_range.start.0 > 0
                            {
                                scope_identifiers.make_set(place.identifier.id);
                                operands.push(place.identifier.id);
                                merged_operand_found = true;
                            }
                        });
                        if merged_operand_found {
                            scope_identifiers.make_set(instr.lvalue.identifier.id);
                            operands.push(instr.lvalue.identifier.id);
                        }
                    }
                }

                // General case (includes CallExpression, FunctionExpression, etc.)
                _ => {
                    let is_func_expr = matches!(
                        &instr.value,
                        InstructionValue::FunctionExpression { .. }
                            | InstructionValue::ObjectMethod { .. }
                    );

                    visitors::for_each_instruction_operand(instr, |place| {
                        if is_mutable(instr.id, &place.identifier.mutable_range)
                            && place.identifier.mutable_range.start.0 > 0
                        {
                            // Match upstream primitive-skip behavior for function captures.
                            if is_func_expr && matches!(place.identifier.type_, Type::Primitive) {
                                return;
                            }
                            scope_identifiers.make_set(place.identifier.id);
                            operands.push(place.identifier.id);
                        }
                    });
                }
            }

            if !operands.is_empty() {
                scope_identifiers.union_all(&operands);
            }
        }
    }

    if enable_treat_function_deps_as_conditional {
        for (decl_id, store_blocks) in &value_block_store_blocks {
            if store_blocks.len() != 2
                || value_block_result_direct_return_counts
                    .get(decl_id)
                    .copied()
                    .unwrap_or(0)
                    != 1
                || value_block_result_outside_use_counts
                    .get(decl_id)
                    .copied()
                    .unwrap_or(0)
                    != 0
            {
                continue;
            }
            let Some(source_ids) = value_block_store_sources.get(decl_id) else {
                continue;
            };
            if !source_ids
                .iter()
                .any(|source_id| conditional_function_call_result_ids.contains(source_id))
                || !source_ids
                    .iter()
                    .any(|source_id| !conditional_function_call_result_ids.contains(source_id))
            {
                continue;
            }

            let mut merged_ids: Vec<IdentifierId> = Vec::new();
            if let Some(target_ids) = value_block_store_targets.get(decl_id) {
                merged_ids.extend(target_ids.iter().copied());
            }
            merged_ids.extend(source_ids.iter().copied());
            for source_id in source_ids {
                if let Some(operand_ids) = conditional_function_call_operand_ids.get(source_id) {
                    merged_ids.extend(operand_ids.iter().copied());
                }
            }
            merged_ids.retain(|id| !load_global_ids.contains(id));
            merged_ids.sort_unstable_by_key(|id| id.0);
            merged_ids.dedup();
            if merged_ids.len() > 1 {
                for id in &merged_ids {
                    scope_identifiers.make_set(*id);
                }
                scope_identifiers.union_all(&merged_ids);
            }
        }
    }

    scope_identifiers
}

/// Compute mutable-alias canonical roots for each identifier in the disjoint set.
///
/// This mirrors upstream use of `findDisjointMutableValues` in
/// `InferReactivePlaces` to canonicalize reactivity across mutably aliased
/// identifiers.
pub fn compute_disjoint_mutable_alias_roots(
    func: &HIRFunction,
) -> HashMap<IdentifierId, IdentifierId> {
    let mut scope_identifiers = find_disjoint_mutable_values(func);
    let mut ids: Vec<IdentifierId> = scope_identifiers.parent.keys().copied().collect();
    ids.sort_by_key(|id| id.0);

    let mut roots = HashMap::with_capacity(ids.len());
    for id in ids {
        roots.insert(id, scope_identifiers.find(id));
    }
    roots
}

// ---------------------------------------------------------------------------
// Range propagation (needed due to Rust value semantics)
// ---------------------------------------------------------------------------

/// Collect the widest mutable range for each IdentifierId across all copies.
///
/// In upstream JS, all copies of an Identifier share the same object reference,
/// so when InferMutationAliasingRanges sets ranges on lvalues, those ranges are
/// visible on every copy. In Rust, identifier copies are independent clones, so
/// we need to explicitly propagate the widest range.
fn collect_identifier_ranges(func: &HIRFunction) -> HashMap<IdentifierId, MutableRange> {
    let mut ranges: HashMap<IdentifierId, MutableRange> = HashMap::new();

    let mut update = |ident: &Identifier| {
        ranges
            .entry(ident.id)
            .and_modify(|existing| {
                if ident.mutable_range.start.0 > 0
                    && (existing.start.0 == 0 || ident.mutable_range.start.0 < existing.start.0)
                {
                    existing.start = ident.mutable_range.start;
                }
                if ident.mutable_range.end.0 > existing.end.0 {
                    existing.end = ident.mutable_range.end;
                }
            })
            .or_insert_with(|| ident.mutable_range.clone());
    };

    for (_bid, block) in &func.body.blocks {
        for phi in &block.phis {
            update(&phi.place.identifier);
            for operand in phi.operands.values() {
                update(&operand.identifier);
            }
        }
        for instr in &block.instructions {
            update(&instr.lvalue.identifier);
            // Do not collect ranges from lowered-function captured contexts here.
            // Those identifiers belong to a different instruction-id numbering space
            // (the nested function body) and can widen parent ranges incorrectly
            // when merged by IdentifierId.
            if !matches!(
                instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            ) {
                visitors::for_each_instruction_operand(instr, |place| update(&place.identifier));
            }
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    update(&lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    visitors::for_each_pattern_place(&lvalue.pattern, &mut |place| {
                        update(&place.identifier);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    update(&lvalue.identifier);
                }
                _ => {}
            }
        }
        // Also collect from terminal operands (return value, throw value, etc.)
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            update(&place.identifier);
        });
    }

    ranges
}

/// Apply collected ranges back to all identifier copies.
fn apply_identifier_ranges(func: &mut HIRFunction, ranges: &HashMap<IdentifierId, MutableRange>) {
    let apply = |ident: &mut Identifier| {
        if let Some(range) = ranges.get(&ident.id) {
            ident.mutable_range = range.clone();
        }
    };

    for (_bid, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            apply(&mut phi.place.identifier);
            for operand in phi.operands.values_mut() {
                apply(&mut operand.identifier);
            }
        }
        for instr in &mut block.instructions {
            apply(&mut instr.lvalue.identifier);
            // Upstream AnalyseFunctions intentionally resets lowered-function
            // context operands to mutableRange [0, 0) before outer scope
            // inference. Do not re-apply canonical outer ranges onto those
            // operands, otherwise we reintroduce pre-reset ranges and over-union
            // function-expression scopes with outer mutable values.
            if !matches!(
                instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            ) {
                visitors::map_instruction_operands(instr, |place| apply(&mut place.identifier));
            }
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    apply(&mut lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    map_pattern_identifiers(&mut lvalue.pattern, &mut |ident| apply(ident));
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    apply(&mut lvalue.identifier);
                }
                _ => {}
            }
        }
        // Also apply to terminal operands
        visitors::map_terminal_operands(&mut block.terminal, |place| {
            apply(&mut place.identifier);
        });
    }
}

// ---------------------------------------------------------------------------
// Main entry points
// ---------------------------------------------------------------------------

/// Infer reactive scope variables for a function (non-aliasing pipeline).
/// Used by analyse_functions for function expression analysis.
pub fn infer_reactive_scope_variables(func: &mut HIRFunction) -> u32 {
    infer_reactive_scope_variables_impl(func, false)
}

/// Infer reactive scope variables using pre-computed aliasing ranges.
/// This is the main pipeline entry point.
pub fn infer_reactive_scope_variables_with_aliasing(func: &mut HIRFunction) -> u32 {
    infer_reactive_scope_variables_impl(func, true)
}

/// Core implementation — exact port of upstream `inferReactiveScopeVariables()`.
fn infer_reactive_scope_variables_impl(func: &mut HIRFunction, use_aliasing_ranges: bool) -> u32 {
    // Step 1: Number instructions (only needed for non-aliasing path,
    // the aliasing path gets its IDs from the pipeline's earlier numbering)
    if !use_aliasing_ranges {
        let _max_id = number_instructions(func);
    }

    // Step 2: Compute or propagate mutable ranges
    if !use_aliasing_ranges {
        // Non-aliasing path: compute ranges from scratch
        infer_mutable_ranges(func);
    } else {
        // Aliasing path: ranges are already set on lvalues by
        // inferMutationAliasingRanges. We need to propagate the widest range
        // across all copies of each IdentifierId (Rust value semantics fix).
        let ranges = collect_identifier_ranges(func);
        apply_identifier_ranges(func, &ranges);
    }

    // Step 3: Find disjoint sets of co-mutating identifiers
    let mut scope_identifiers = find_disjoint_mutable_values(func);

    // Step 4: Create ReactiveScope for each disjoint set
    //
    // Port of upstream's scopeIdentifiers.forEach() loop which:
    // - Creates a new scope when encountering a new group
    // - Merges mutable ranges across all identifiers in a group
    // - Sets identifier.scope and identifier.mutableRange for each identifier
    let mut scopes: HashMap<IdentifierId, ReactiveScope> = HashMap::new();
    let mut next_scope_id = ScopeId::new(0);

    // Since we can't look up Identifier objects by IdentifierId during iteration
    // (Rust ownership), we need a two-pass approach:
    //
    // Pass A: Collect mutable ranges for all identifiers in the union-find
    let mut id_ranges: HashMap<IdentifierId, MutableRange> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        for phi in &block.phis {
            if scope_identifiers
                .parent
                .contains_key(&phi.place.identifier.id)
            {
                id_ranges
                    .entry(phi.place.identifier.id)
                    .and_modify(|r| merge_range(r, &phi.place.identifier.mutable_range))
                    .or_insert_with(|| phi.place.identifier.mutable_range.clone());
            }
            for op in phi.operands.values() {
                if scope_identifiers.parent.contains_key(&op.identifier.id) {
                    id_ranges
                        .entry(op.identifier.id)
                        .and_modify(|r| merge_range(r, &op.identifier.mutable_range))
                        .or_insert_with(|| op.identifier.mutable_range.clone());
                }
            }
        }
        for instr in &block.instructions {
            let mut collect = |ident: &Identifier| {
                if scope_identifiers.parent.contains_key(&ident.id) {
                    id_ranges
                        .entry(ident.id)
                        .and_modify(|r| merge_range(r, &ident.mutable_range))
                        .or_insert_with(|| ident.mutable_range.clone());
                }
            };
            collect(&instr.lvalue.identifier);
            visitors::for_each_instruction_operand(instr, |place| collect(&place.identifier));
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    collect(&lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    visitors::for_each_pattern_place(&lvalue.pattern, &mut |place| {
                        collect(&place.identifier);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    collect(&lvalue.identifier);
                }
                _ => {}
            }
        }
    }

    // Pass B: Build scopes, mirroring upstream's forEach pattern.
    // Iterate each identifier in the union-find, find its group root, and either
    // create a new scope for that group or extend the existing scope's range.
    // Sort by IdentifierId for deterministic output (upstream uses Map which
    // preserves insertion order; Rust HashMap does not).
    let mut all_ids: Vec<IdentifierId> = scope_identifiers.parent.keys().copied().collect();
    all_ids.sort_by_key(|id| id.0);
    for id in all_ids {
        let group_id = scope_identifiers.find(id);
        let ident_range = match id_ranges.get(&id) {
            Some(r) => r.clone(),
            None => continue,
        };

        if let Some(scope) = scopes.get_mut(&group_id) {
            // Extend existing scope — upstream range merge logic
            if scope.range.start.0 == 0 {
                scope.range.start = ident_range.start;
            } else if ident_range.start.0 != 0 {
                scope.range.start =
                    InstructionId::new(scope.range.start.0.min(ident_range.start.0));
            }
            scope.range.end = InstructionId::new(scope.range.end.0.max(ident_range.end.0));
        } else {
            // Create new scope for this group
            let scope = ReactiveScope {
                id: next_scope_id,
                range: ident_range.clone(),
                dependencies: Vec::new(),
                declarations: IndexMap::new(),
                reassignments: Vec::new(),
                merged_id: None,
                early_return_value: None,
            };
            scopes.insert(group_id, scope);
            next_scope_id = ScopeId::new(next_scope_id.0 + 1);
        }
    }

    // Compute maxInstruction for validation
    let mut max_instruction: u32 = 0;
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            max_instruction = max_instruction.max(instr.id.0);
        }
        max_instruction = max_instruction.max(get_terminal_id(&block.terminal).0);
    }

    // Validate scope ranges (matching upstream validation)
    // Upstream checks: start === 0 || end === 0 || maxInstruction === 0 || end > maxInstruction + 1
    scopes.retain(|_, scope| {
        if scope.range.start.0 == 0
            || scope.range.end.0 == 0
            || max_instruction == 0
            || scope.range.end.0 > max_instruction + 1
        {
            return false;
        }
        true
    });

    let num_scopes = next_scope_id.0;

    // Step 5: Build a direct IdentifierId -> ReactiveScope lookup
    // Pre-resolve all group roots so we don't need mutable access to
    // scope_identifiers during the assignment pass.
    let mut id_to_scope: HashMap<IdentifierId, ReactiveScope> = HashMap::new();
    let mut all_ids2: Vec<IdentifierId> = scope_identifiers.parent.keys().copied().collect();
    all_ids2.sort_by_key(|id| id.0);
    for id in all_ids2 {
        let group_id = scope_identifiers.find(id);
        if let Some(scope) = scopes.get(&group_id) {
            id_to_scope.insert(id, scope.clone());
        }
    }

    if std::env::var("DEBUG_INFER_SCOPE_VARIABLES").is_ok() {
        let id_to_name = build_name_lookup(func);
        type ScopeDebugEntry = (u32, u32, String, u32, u32);
        let mut by_scope: BTreeMap<u32, Vec<ScopeDebugEntry>> = BTreeMap::new();

        for (id, scope) in &id_to_scope {
            let ident_range = id_ranges.get(id).cloned().unwrap_or(MutableRange {
                start: InstructionId::new(0),
                end: InstructionId::new(0),
            });
            let name = id_to_name
                .get(id)
                .cloned()
                .unwrap_or_else(|| "<anon>".to_string());
            by_scope.entry(scope.id.0).or_default().push((
                id.0,
                scope.id.0,
                name,
                ident_range.start.0,
                ident_range.end.0,
            ));
        }

        eprintln!(
            "[INFER_SCOPE_VARIABLES] fn={} scopes={}",
            func.id.as_deref().unwrap_or("<anonymous>"),
            by_scope.len()
        );
        for (scope_id, members) in &mut by_scope {
            members.sort_by_key(|(id, _, _, _, _)| *id);
            let scope_range = members.iter().fold((u32::MAX, 0), |acc, m| {
                let start = acc.0.min(m.3);
                let end = acc.1.max(m.4);
                (start, end)
            });
            eprintln!(
                "  scope={} merged_range=({}, {}) members={}",
                scope_id,
                scope_range.0,
                scope_range.1,
                members.len()
            );
            for (id, _sid, name, start, end) in members {
                eprintln!("    id={} name={} range=({}, {})", id, name, start, end);
            }
        }

        eprintln!("  instructions:");
        for (bid, block) in &func.body.blocks {
            for instr in &block.instructions {
                let name = match &instr.lvalue.identifier.name {
                    Some(IdentifierName::Named(s)) | Some(IdentifierName::Promoted(s)) => {
                        s.as_str()
                    }
                    None => "<anon>",
                };
                eprintln!(
                    "    bb{} instr#{} lvalue_id={} name={} range=({}, {}) value={:?}",
                    bid.0,
                    instr.id.0,
                    instr.lvalue.identifier.id.0,
                    name,
                    instr.lvalue.identifier.mutable_range.start.0,
                    instr.lvalue.identifier.mutable_range.end.0,
                    instr.value
                );
            }
        }
    }

    // Step 6: Assign scopes to identifiers
    for (_bid, block) in &mut func.body.blocks {
        for phi in &mut block.phis {
            assign_scope(&id_to_scope, &mut phi.place.identifier);
            for op in phi.operands.values_mut() {
                assign_scope(&id_to_scope, &mut op.identifier);
            }
        }
        for instr in &mut block.instructions {
            assign_scope(&id_to_scope, &mut instr.lvalue.identifier);
            // Upstream assigns scopes to the exact Identifier objects stored in
            // the disjoint set. In Rust we key by IdentifierId, so blindly
            // reassigning scopes onto lowered-function captured contexts would
            // splash outer scopes back onto copies that AnalyseFunctions/reset
            // intentionally left at [0, 0), reintroducing over-union during
            // overlap merging.
            if !matches!(
                instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            ) {
                visitors::map_instruction_operands(instr, |place| {
                    assign_scope(&id_to_scope, &mut place.identifier);
                });
            }
            match &mut instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    assign_scope(&id_to_scope, &mut lvalue.place.identifier);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    map_pattern_identifiers(&mut lvalue.pattern, &mut |ident| {
                        assign_scope(&id_to_scope, ident);
                    });
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    assign_scope(&id_to_scope, &mut lvalue.identifier);
                }
                _ => {}
            }
        }
    }

    num_scopes
}

/// Merge a mutable range into an existing one, taking the wider bounds.
fn merge_range(existing: &mut MutableRange, new: &MutableRange) {
    if new.start.0 > 0 && (existing.start.0 == 0 || new.start.0 < existing.start.0) {
        existing.start = new.start;
    }
    if new.end.0 > existing.end.0 {
        existing.end = new.end;
    }
}

/// Assign a scope to an identifier from the pre-resolved lookup.
fn assign_scope(id_to_scope: &HashMap<IdentifierId, ReactiveScope>, ident: &mut Identifier) {
    if let Some(scope) = id_to_scope.get(&ident.id) {
        ident.scope = Some(Box::new(scope.clone()));
        ident.mutable_range = scope.range.clone();
    }
}
