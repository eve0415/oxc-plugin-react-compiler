//! Infer mutation and aliasing effects for instructions and terminals.
//!
//! Port of `InferMutationAliasingEffects.ts` from upstream React Compiler
//! (babel-plugin-react-compiler). Copyright (c) Meta Platforms, Inc. and affiliates.
//! Licensed under MIT.
//!
//! This pass performs abstract interpretation over the HIR CFG until a fixpoint is reached.
//! For each instruction, it computes candidate effects (AliasingEffect variants) based on
//! the instruction's syntax/type, then applies those effects to an abstract state that
//! tracks:
//!   - The abstract "kind" of each value (Mutable, Frozen, Primitive, Global, Context, MaybeFrozen)
//!   - Which identifiers point to which abstract values
//! The resolved effects are written onto each instruction's `effects` field.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::error::{CompilerDiagnostic, DiagnosticSeverity};
use crate::hir::builder::terminal_successors;
use crate::hir::object_shape::{
    FunctionSignature, HookKind, ReturnType, TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
};
use crate::hir::types::*;
use crate::hir::visitors;
use crate::inference::aliasing_effects::*;

// ---------------------------------------------------------------------------
// AbstractValue
// ---------------------------------------------------------------------------

/// The abstract "kind" of a value at a given program point, plus the reasons
/// why the value has that kind.
#[derive(Debug, Clone)]
struct AbstractValue {
    kind: ValueKind,
    reasons: HashSet<ValueReason>,
}

impl AbstractValue {
    fn new(kind: ValueKind, reason: ValueReason) -> Self {
        let mut reasons = HashSet::new();
        reasons.insert(reason);
        Self { kind, reasons }
    }

    fn with_reasons(kind: ValueKind, reasons: HashSet<ValueReason>) -> Self {
        Self { kind, reasons }
    }

    fn first_reason(&self) -> ValueReason {
        self.reasons
            .iter()
            .next()
            .copied()
            .unwrap_or(ValueReason::Other)
    }
}

// ---------------------------------------------------------------------------
// ValueId: opaque identifier for abstract values (replaces pointer identity)
// ---------------------------------------------------------------------------

/// An opaque identifier for an abstract value in the inference state.
/// The upstream TypeScript uses object identity (InstructionValue references)
/// to distinguish values. We use integer IDs instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ValueId(u32);

/// Signature metadata attached to locally-created function values.
#[derive(Debug, Clone)]
struct StoredFunctionSignature {
    signature: AliasingSignature,
    context: Vec<Place>,
}

// ---------------------------------------------------------------------------
// InferenceState
// ---------------------------------------------------------------------------

/// The abstract state during inference: maps value IDs to their abstract kind,
/// and maps identifier IDs to the set of value IDs they may point to.
#[derive(Debug, Clone)]
struct InferenceState {
    is_function_expression: bool,
    is_inferred_memo_enabled: bool,
    validate_no_impure_functions_in_render: bool,
    enable_preserve_existing_memoization_guarantees: bool,
    enable_assume_hooks_follow_rules_of_react: bool,
    enable_transitively_freeze_function_expressions: bool,
    /// Maps value IDs to their abstract kind.
    values: HashMap<ValueId, AbstractValue>,
    /// Maps function value IDs to a local aliasing signature.
    function_signatures: HashMap<ValueId, StoredFunctionSignature>,
    /// Maps function value IDs to their captured context places.
    function_contexts: HashMap<ValueId, Vec<Place>>,
    /// Maps each SSA identifier to the set of abstract values it may point to.
    variables: HashMap<IdentifierId, HashSet<ValueId>>,
    /// Counter for generating fresh value IDs.
    next_value_id: u32,
}

impl InferenceState {
    fn empty(
        is_function_expression: bool,
        is_inferred_memo_enabled: bool,
        validate_no_impure_functions_in_render: bool,
        enable_preserve_existing_memoization_guarantees: bool,
        enable_assume_hooks_follow_rules_of_react: bool,
        enable_transitively_freeze_function_expressions: bool,
    ) -> Self {
        Self {
            is_function_expression,
            is_inferred_memo_enabled,
            validate_no_impure_functions_in_render,
            enable_preserve_existing_memoization_guarantees,
            enable_assume_hooks_follow_rules_of_react,
            enable_transitively_freeze_function_expressions,
            values: HashMap::new(),
            function_signatures: HashMap::new(),
            function_contexts: HashMap::new(),
            variables: HashMap::new(),
            next_value_id: 0,
        }
    }

    /// Allocate a fresh value ID.
    fn fresh_value_id(&mut self) -> ValueId {
        let id = ValueId(self.next_value_id);
        self.next_value_id += 1;
        id
    }

    /// Initialize a new abstract value and return its ID.
    fn initialize(&mut self, kind: AbstractValue) -> ValueId {
        let id = self.fresh_value_id();
        self.values.insert(id, kind);
        id
    }

    /// Initialize (or overwrite) an abstract value tied to a concrete place ID.
    ///
    /// Using stable IDs avoids cross-branch collisions where different branch-local
    /// temporaries accidentally share a fresh value ID and get merged together.
    fn initialize_for_place(&mut self, place_id: IdentifierId, kind: AbstractValue) -> ValueId {
        let id = ValueId(place_id.0);
        self.values.insert(id, kind);
        self.next_value_id = self.next_value_id.max(id.0 + 1);
        id
    }

    /// Define (or redefine) a variable to point to a specific value.
    fn define(&mut self, place_id: IdentifierId, value_id: ValueId) {
        let mut set = HashSet::new();
        set.insert(value_id);
        self.variables.insert(place_id, set);
    }

    /// Check if a variable is defined.
    fn is_defined(&self, place_id: IdentifierId) -> bool {
        self.variables.contains_key(&place_id)
    }

    /// Look up the merged abstract kind of a variable (merging across all values it may alias).
    fn kind(&self, place_id: IdentifierId) -> AbstractValue {
        let values = self.variables.get(&place_id);
        match values {
            Some(val_ids) => {
                let mut merged: Option<AbstractValue> = None;
                for vid in val_ids {
                    if let Some(v) = self.values.get(vid) {
                        merged = Some(match merged {
                            Some(prev) => merge_abstract_values(&prev, v),
                            None => v.clone(),
                        });
                    }
                }
                merged.unwrap_or_else(|| AbstractValue::new(ValueKind::Mutable, ValueReason::Other))
            }
            None => {
                // If not defined, treat as mutable (conservative).
                AbstractValue::new(ValueKind::Mutable, ValueReason::Other)
            }
        }
    }

    /// Associate local function signature metadata with an abstract value.
    fn set_function_signature(&mut self, value_id: ValueId, signature: StoredFunctionSignature) {
        self.function_signatures.insert(value_id, signature);
    }

    fn set_function_contexts(&mut self, value_id: ValueId, context: Vec<Place>) {
        self.function_contexts.insert(value_id, context);
    }

    /// Resolve a local function signature for a place if it points to exactly one value.
    fn local_function_signature(&self, place_id: IdentifierId) -> Option<&StoredFunctionSignature> {
        let values = self.variables.get(&place_id)?;
        if values.len() != 1 {
            return None;
        }
        let value_id = values.iter().next()?;
        self.function_signatures.get(value_id)
    }

    fn function_contexts_for_place(&self, place_id: IdentifierId) -> Option<Vec<Place>> {
        let values = self.variables.get(&place_id)?;
        if values.len() != 1 {
            return None;
        }
        let value_id = values.iter().next()?;
        self.function_contexts.get(value_id).cloned()
    }

    /// Assign: make `place_id` point to the same values as `from_id`.
    fn assign(&mut self, place_id: IdentifierId, from_id: IdentifierId) {
        if let Some(vals) = self.variables.get(&from_id).cloned() {
            self.variables.insert(place_id, vals);
        }
    }

    /// Append alias: add the values of `from_id` to `place_id`'s value set.
    #[allow(dead_code)]
    fn append_alias(&mut self, place_id: IdentifierId, from_id: IdentifierId) {
        if let Some(from_vals) = self.variables.get(&from_id).cloned() {
            let entry = self.variables.entry(place_id).or_default();
            for v in from_vals {
                entry.insert(v);
            }
        }
    }

    /// Freeze a variable: set all its values to Frozen.
    /// Returns true if the value was not already frozen.
    fn freeze(&mut self, place_id: IdentifierId, reason: ValueReason) -> bool {
        let kind = self.kind(place_id);
        match kind.kind {
            ValueKind::Context | ValueKind::Mutable | ValueKind::MaybeFrozen => {
                if let Some(val_ids) = self.variables.get(&place_id).cloned() {
                    for vid in val_ids {
                        self.freeze_value(vid, reason);
                    }
                }
                true
            }
            ValueKind::Frozen | ValueKind::Global | ValueKind::Primitive => false,
        }
    }

    /// Freeze a single value by its ID.
    fn freeze_value(&mut self, value_id: ValueId, reason: ValueReason) {
        let mut reasons = HashSet::new();
        reasons.insert(reason);
        self.values.insert(
            value_id,
            AbstractValue::with_reasons(ValueKind::Frozen, reasons),
        );

        // Upstream: optionally freeze captured context when freezing function values.
        if (self.enable_preserve_existing_memoization_guarantees
            || self.enable_transitively_freeze_function_expressions)
            && let Some(context_places) = self.function_contexts.get(&value_id).cloned()
        {
            for place in context_places {
                self.freeze(place.identifier.id, reason);
            }
        }
    }

    /// Determine the mutation result for a given place.
    fn mutate(&self, variant: MutationVariant, place: &Place) -> MutationResult {
        let debug_mutate = std::env::var("DEBUG_MUTATE_KIND").is_ok();
        let is_ref_like = is_ref_or_ref_value(&place.identifier);
        if debug_mutate {
            eprintln!(
                "[MUTATE_KIND] variant={:?} id={} decl={} name={:?} ident_type={:?} kind={:?} is_ref_like={}",
                variant,
                place.identifier.id.0,
                place.identifier.declaration_id.0,
                place.identifier.name,
                place.identifier.type_,
                self.kind(place.identifier.id).kind,
                is_ref_like
            );
        }
        // Upstream treats writes to refs/ref-like values as `mutate-ref` regardless
        // of preserve-existing-memoization flags.
        if is_ref_like {
            if debug_mutate {
                eprintln!("[MUTATE_KIND] -> MutateRef");
            }
            return MutationResult::MutateRef;
        }
        let kind = self.kind(place.identifier.id);
        let result = match variant {
            MutationVariant::MutateConditionally
            | MutationVariant::MutateTransitiveConditionally => match kind.kind {
                ValueKind::Mutable | ValueKind::Context => MutationResult::Mutate,
                _ => MutationResult::None,
            },
            MutationVariant::Mutate | MutationVariant::MutateTransitive => match kind.kind {
                ValueKind::Mutable | ValueKind::Context => MutationResult::Mutate,
                ValueKind::Primitive => MutationResult::None,
                ValueKind::Frozen => MutationResult::MutateFrozen,
                ValueKind::Global => MutationResult::MutateGlobal,
                ValueKind::MaybeFrozen => MutationResult::MutateFrozen,
            },
        };
        if debug_mutate {
            eprintln!("[MUTATE_KIND] -> {:?}", result);
        }
        result
    }

    /// Merge another state into this one. Returns a new state if there were changes,
    /// or None if this state already subsumes `other`.
    fn merge(&self, other: &InferenceState) -> Option<InferenceState> {
        let mut next_values: Option<HashMap<ValueId, AbstractValue>> = None;
        let mut next_function_signatures: Option<HashMap<ValueId, StoredFunctionSignature>> = None;
        let mut next_variables: Option<HashMap<IdentifierId, HashSet<ValueId>>> = None;

        // Merge values present in both states
        for (id, this_val) in &self.values {
            if let Some(other_val) = other.values.get(id) {
                let merged = merge_abstract_values(this_val, other_val);
                if merged.kind != this_val.kind
                    || !is_superset(&this_val.reasons, &other_val.reasons)
                {
                    let nv = next_values.get_or_insert_with(|| self.values.clone());
                    nv.insert(*id, merged);
                }
            }
        }
        // Add values only in other
        for (id, other_val) in &other.values {
            if !self.values.contains_key(id) {
                let nv = next_values.get_or_insert_with(|| self.values.clone());
                nv.insert(*id, other_val.clone());
            }
        }

        // Merge known function signatures by value ID.
        for (id, other_sig) in &other.function_signatures {
            if !self.function_signatures.contains_key(id) {
                let nfs = next_function_signatures
                    .get_or_insert_with(|| self.function_signatures.clone());
                nfs.insert(*id, other_sig.clone());
            }
        }

        let mut next_function_contexts: Option<HashMap<ValueId, Vec<Place>>> = None;
        for (id, other_ctx) in &other.function_contexts {
            if !self.function_contexts.contains_key(id) {
                let nfc =
                    next_function_contexts.get_or_insert_with(|| self.function_contexts.clone());
                nfc.insert(*id, other_ctx.clone());
            }
        }

        // Merge variables present in both
        for (id, this_vals) in &self.variables {
            if let Some(other_vals) = other.variables.get(id) {
                let mut has_new = false;
                for v in other_vals {
                    if !this_vals.contains(v) {
                        has_new = true;
                        break;
                    }
                }
                if has_new {
                    let nvar = next_variables.get_or_insert_with(|| self.variables.clone());
                    let entry = nvar.entry(*id).or_insert_with(|| this_vals.clone());
                    for v in other_vals {
                        entry.insert(*v);
                    }
                }
            }
        }
        // Add variables only in other
        for (id, other_vals) in &other.variables {
            if !self.variables.contains_key(id) {
                let nvar = next_variables.get_or_insert_with(|| self.variables.clone());
                nvar.insert(*id, other_vals.clone());
            }
        }

        if next_values.is_none()
            && next_function_signatures.is_none()
            && next_function_contexts.is_none()
            && next_variables.is_none()
        {
            None
        } else {
            let max_id = std::cmp::max(self.next_value_id, other.next_value_id);
            Some(InferenceState {
                is_function_expression: self.is_function_expression,
                is_inferred_memo_enabled: self.is_inferred_memo_enabled,
                validate_no_impure_functions_in_render: self.validate_no_impure_functions_in_render,
                enable_preserve_existing_memoization_guarantees: self
                    .enable_preserve_existing_memoization_guarantees,
                enable_assume_hooks_follow_rules_of_react: self
                    .enable_assume_hooks_follow_rules_of_react,
                enable_transitively_freeze_function_expressions: self
                    .enable_transitively_freeze_function_expressions,
                values: next_values.unwrap_or_else(|| self.values.clone()),
                function_signatures: next_function_signatures
                    .unwrap_or_else(|| self.function_signatures.clone()),
                function_contexts: next_function_contexts
                    .unwrap_or_else(|| self.function_contexts.clone()),
                variables: next_variables.unwrap_or_else(|| self.variables.clone()),
                next_value_id: max_id,
            })
        }
    }

    /// Process a phi node: merge the values from all operands that have been defined.
    fn infer_phi(&mut self, phi: &Phi) {
        let mut merged_values: HashSet<ValueId> = HashSet::new();
        for operand in phi.operands.values() {
            if let Some(vals) = self.variables.get(&operand.identifier.id) {
                for v in vals {
                    merged_values.insert(*v);
                }
            }
            // operand not yet defined → backedge, will be handled by merge
        }
        if !merged_values.is_empty() {
            self.variables
                .insert(phi.place.identifier.id, merged_values);
        }
    }
}

// ---------------------------------------------------------------------------
// MutationVariant / MutationResult
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum MutationVariant {
    Mutate,
    MutateConditionally,
    MutateTransitive,
    MutateTransitiveConditionally,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MutationResult {
    None,
    Mutate,
    MutateRef,
    MutateFrozen,
    MutateGlobal,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Per-function context for the inference pass.
struct InferContext {
    is_function_expression: bool,
    /// Map from catch handler block ID to handler binding place.
    catch_handlers_by_handler_block: HashMap<BlockId, Place>,
    /// Map from try-body block ID to catch handler binding place.
    /// This mirrors upstream `maybe-throw` handling when CFG simplification
    /// lowers try sub-blocks to plain gotos before this pass runs.
    catch_handlers_by_try_block: HashMap<BlockId, Place>,
    /// Map from try-body block ID to catch handler block ID for synthetic
    /// throw-edge propagation during fixpoint state queuing.
    catch_handler_successor_by_try_block: HashMap<BlockId, BlockId>,
    /// Declaration IDs of hoisted context declarations
    hoisted_context_declarations: HashSet<DeclarationId>,
    /// Outer-scope declaration IDs captured by nested functions with Capture effects.
    ///
    /// Upstream BuildHIR materializes these as context declarations before this pass.
    /// Our port records them here so local declaration kind can approximate that behavior.
    captured_context_declarations: HashSet<DeclarationId>,
    /// Stable context place for captured declarations.
    ///
    /// Upstream represents captured mutable locals as context variables. Our lowered
    /// HIR may still emit StoreLocal/LoadLocal with SSA-renamed ids, so use a stable
    /// context place (first captured context operand) for parity.
    captured_context_place_by_declaration: HashMap<DeclarationId, Place>,
    /// String literal declarations used as method keys (e.g. `"concat"`).
    method_name_by_declaration: HashMap<DeclarationId, String>,
    /// LoadGlobal declarations to their global names.
    global_name_by_declaration: HashMap<DeclarationId, String>,
    /// Local declaration flow for LoadLocal/LoadContext/TypeCastExpression.
    load_source_by_declaration: HashMap<DeclarationId, DeclarationId>,
    /// Declarations whose associated function expression can return JSX.
    jsx_returning_function_declarations: HashSet<DeclarationId>,
    /// Best-effort declaration type hints collected from identifier copies.
    declaration_type_by_declaration: HashMap<DeclarationId, Type>,
    /// Declaration ids that are local to the current function body.
    /// Reassignments to named declarations outside this set are treated as global writes.
    local_declarations: HashSet<DeclarationId>,
    /// Declarations that are reassigned after their initial definition.
    reassigned_declarations: HashSet<DeclarationId>,
}

// ---------------------------------------------------------------------------
// Merge helpers
// ---------------------------------------------------------------------------

fn is_superset(a: &HashSet<ValueReason>, b: &HashSet<ValueReason>) -> bool {
    b.iter().all(|r| a.contains(r))
}

/// Merge two abstract values according to the lattice rules.
fn merge_abstract_values(a: &AbstractValue, b: &AbstractValue) -> AbstractValue {
    let kind = merge_value_kinds(a.kind, b.kind);
    if kind == a.kind && kind == b.kind && is_superset(&a.reasons, &b.reasons) {
        return a.clone();
    }
    let mut reasons = a.reasons.clone();
    for r in &b.reasons {
        reasons.insert(*r);
    }
    AbstractValue { kind, reasons }
}

/// Join lattice for ValueKind.
///
/// See upstream comments for the full lattice diagram. Key rules:
/// - immutable | mutable => mutable
/// - frozen | mutable => maybe-frozen
/// - immutable | frozen => frozen
/// - <any> | maybe-frozen => maybe-frozen
/// - immutable | context => context
/// - mutable | context => context
/// - frozen | context => maybe-frozen
fn merge_value_kinds(a: ValueKind, b: ValueKind) -> ValueKind {
    if a == b {
        return a;
    }
    if a == ValueKind::MaybeFrozen || b == ValueKind::MaybeFrozen {
        return ValueKind::MaybeFrozen;
    }
    if a == ValueKind::Mutable || b == ValueKind::Mutable {
        if a == ValueKind::Frozen || b == ValueKind::Frozen {
            return ValueKind::MaybeFrozen;
        }
        if a == ValueKind::Context || b == ValueKind::Context {
            return ValueKind::Context;
        }
        return ValueKind::Mutable;
    }
    if a == ValueKind::Context || b == ValueKind::Context {
        if a == ValueKind::Frozen || b == ValueKind::Frozen {
            return ValueKind::MaybeFrozen;
        }
        return ValueKind::Context;
    }
    if a == ValueKind::Frozen || b == ValueKind::Frozen {
        return ValueKind::Frozen;
    }
    if a == ValueKind::Global || b == ValueKind::Global {
        return ValueKind::Global;
    }
    ValueKind::Primitive
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Infer mutation/aliasing effects for all instructions and terminals in `func`.
///
/// This is the main pass entry point. It:
/// 1. Initializes abstract state for parameters and context variables
/// 2. Iterates over blocks in order until fixpoint
/// 3. For each instruction, computes candidate effects and applies them
/// 4. Writes the resolved effects onto each instruction's `effects` field
pub fn infer_mutation_aliasing_effects(
    func: &mut HIRFunction,
    is_function_expression: bool,
    is_inferred_memo_enabled: bool,
) {
    let initial_state = InferenceState::empty(
        is_function_expression,
        is_inferred_memo_enabled,
        func.env.config().validate_no_impure_functions_in_render,
        func.env
            .config()
            .enable_preserve_existing_memoization_guarantees,
        func.env.config().enable_assume_hooks_follow_rules_of_react,
        func.env
            .config()
            .enable_transitively_freeze_function_expressions,
    );

    // Build context
    let hoisted = find_hoisted_context_declarations(func);
    let (captured_context_declarations, captured_context_place_by_declaration) =
        find_captured_context_declarations(func);
    let method_name_by_declaration = collect_string_literal_declarations(func);
    let global_name_by_declaration = collect_load_global_declarations(func);
    let load_source_by_declaration = collect_load_source_declarations(func);
    let jsx_returning_function_declarations = collect_jsx_returning_function_declarations(func);
    let declaration_type_by_declaration = collect_declaration_type_hints(func);
    let local_declarations = collect_local_declarations(func);
    let reassigned_declarations = collect_reassigned_declarations(func);
    let (
        catch_handlers_by_handler_block,
        catch_handlers_by_try_block,
        catch_handler_successor_by_try_block,
    ) = collect_catch_handlers(func);
    let ctx = InferContext {
        is_function_expression,
        catch_handlers_by_handler_block,
        catch_handlers_by_try_block,
        catch_handler_successor_by_try_block,
        hoisted_context_declarations: hoisted,
        captured_context_declarations,
        captured_context_place_by_declaration,
        method_name_by_declaration,
        global_name_by_declaration,
        load_source_by_declaration,
        jsx_returning_function_declarations,
        declaration_type_by_declaration,
        local_declarations,
        reassigned_declarations,
    };

    if std::env::var("DEBUG_APPLY_SIGNATURE").is_ok() {
        let mut entries: Vec<_> = ctx.global_name_by_declaration.iter().collect();
        entries.sort_by_key(|(decl, _)| decl.0);
        if !entries.is_empty() {
            let rendered = entries
                .iter()
                .map(|(decl, name)| format!("{}={}", decl.0, name))
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!("[APPLY_SIG_GLOBALS] {}", rendered);
        }
    }

    // We'll keep states-by-block and queued-states
    let mut states_by_block: HashMap<BlockId, InferenceState> = HashMap::new();
    let mut queued_states: HashMap<BlockId, InferenceState> = HashMap::new();

    // Initialize parameters and context in the initial state
    let mut state = initial_state;
    initialize_params_and_context(func, &mut state, is_function_expression);

    // Queue the entry block
    queued_states.insert(func.body.entry, state);

    // Fixpoint iteration
    let mut iteration_count = 0;
    while !queued_states.is_empty() {
        iteration_count += 1;
        if iteration_count > 100 {
            // Potential infinite loop — bail out
            break;
        }

        // Take the current queued states so we can iterate blocks
        let current_queued = std::mem::take(&mut queued_states);

        for (block_id, block) in &mut func.body.blocks {
            let incoming_state = match current_queued.get(block_id) {
                Some(s) => s,
                None => continue,
            };

            states_by_block.insert(*block_id, incoming_state.clone());
            let mut block_state = incoming_state.clone();

            infer_block(&ctx, &mut block_state, *block_id, block);

            // Queue successors
            let mut successors = terminal_successors(&block.terminal);
            if let Some(handler_succ) = ctx.catch_handler_successor_by_try_block.get(block_id)
                && !successors.contains(handler_succ)
            {
                successors.push(*handler_succ);
            }
            for succ_id in successors {
                queue_state(&states_by_block, &mut queued_states, succ_id, &block_state);
            }
        }
    }
}

/// Queue a state for a successor block. Merges with any existing queued state
/// and checks for changes relative to the last processed state.
fn queue_state(
    states_by_block: &HashMap<BlockId, InferenceState>,
    queued_states: &mut HashMap<BlockId, InferenceState>,
    block_id: BlockId,
    state: &InferenceState,
) {
    if let Some(existing) = queued_states.get(&block_id) {
        // Merge with existing queued state
        if let Some(merged) = existing.merge(state) {
            queued_states.insert(block_id, merged);
        }
        // else: no changes, nothing to do
    } else {
        // First time seeing this block in this iteration
        if let Some(prev_state) = states_by_block.get(&block_id) {
            // Check if there are changes relative to last processed state
            if let Some(merged) = prev_state.merge(state) {
                queued_states.insert(block_id, merged);
            }
            // else: no changes
        } else {
            // Never processed, queue it
            queued_states.insert(block_id, state.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Hoisted context declarations
// ---------------------------------------------------------------------------

fn find_hoisted_context_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut hoisted = HashSet::new();
    let debug_hoisted = std::env::var("DEBUG_BAILOUT_REASON").is_ok();

    let mut first_decl_write: HashMap<DeclarationId, InstructionId> = HashMap::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    let decl = lvalue.place.identifier.declaration_id;
                    first_decl_write
                        .entry(decl)
                        .and_modify(|start| *start = (*start).min(instr.id))
                        .or_insert(instr.id);
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    for_each_pattern_item(&lvalue.pattern, |place, _| {
                        let decl = place.identifier.declaration_id;
                        first_decl_write
                            .entry(decl)
                            .and_modify(|start| *start = (*start).min(instr.id))
                            .or_insert(instr.id);
                    });
                }
                _ => {}
            }
        }
    }

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::DeclareContext { lvalue, .. } = &instr.value {
                let kind = lvalue.kind;
                if kind == InstructionKind::HoistedConst
                    || kind == InstructionKind::HoistedFunction
                    || kind == InstructionKind::HoistedLet
                {
                    hoisted.insert(lvalue.place.identifier.declaration_id);
                }
            } else if let InstructionValue::FunctionExpression { lowered_func, .. }
            | InstructionValue::ObjectMethod { lowered_func, .. } = &instr.value
            {
                // Upstream lowers reads-before-declare through hoisted DeclareContext.
                // Our HIR currently misses some of those lowered declarations, so we
                // conservatively recover equivalent hoisted context declarations by
                // checking captured outer bindings referenced before their first write.
                for operand in &lowered_func.func.context {
                    if debug_hoisted {
                        let first_write = first_decl_write
                            .get(&operand.identifier.declaration_id)
                            .map(|id| id.0);
                        eprintln!(
                            "[HOISTED_CTX_SCAN] instr={} decl={} id={} first_write={:?} start={} end={} name={}",
                            instr.id.0,
                            operand.identifier.declaration_id.0,
                            operand.identifier.id.0,
                            first_write,
                            operand.identifier.mutable_range.start.0,
                            operand.identifier.mutable_range.end.0,
                            operand
                                .identifier
                                .name
                                .as_ref()
                                .map_or("<none>".to_string(), |n| n.value().to_string())
                        );
                    }
                    if first_decl_write
                        .get(&operand.identifier.declaration_id)
                        .is_some_and(|start| instr.id < *start)
                    {
                        hoisted.insert(operand.identifier.declaration_id);
                        if debug_hoisted {
                            let first_write = first_decl_write
                                .get(&operand.identifier.declaration_id)
                                .map_or(0, |id| id.0);
                            eprintln!(
                                "[HOISTED_CTX_DETECT] decl={} reason=context_use_before_decl use_instr={} decl_start={}",
                                operand.identifier.declaration_id.0, instr.id.0, first_write
                            );
                        }
                    }
                }
            }
        }
    }
    if debug_hoisted {
        let mut list: Vec<u32> = hoisted.iter().map(|d| d.0).collect();
        list.sort_unstable();
        eprintln!("[HOISTED_CTX_SET] decls={:?}", list);
    }
    hoisted
}

fn find_captured_context_declarations(
    func: &HIRFunction,
) -> (HashSet<DeclarationId>, HashMap<DeclarationId, Place>) {
    fn collect_from_function(
        func: &HIRFunction,
        out: &mut HashSet<DeclarationId>,
        by_decl: &mut HashMap<DeclarationId, Place>,
    ) {
        for (_block_id, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        for operand in &lowered_func.func.context {
                            if operand.effect == Effect::Capture {
                                let decl = operand.identifier.declaration_id;
                                out.insert(decl);
                                by_decl.entry(decl).or_insert_with(|| operand.clone());
                            }
                        }
                        collect_from_function(&lowered_func.func, out, by_decl);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut decls = HashSet::new();
    let mut places = HashMap::new();
    collect_from_function(func, &mut decls, &mut places);
    (decls, places)
}

fn collect_string_literal_declarations(func: &HIRFunction) -> HashMap<DeclarationId, String> {
    let mut out: HashMap<DeclarationId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(name),
                    ..
                } => {
                    out.insert(instr.lvalue.identifier.declaration_id, name.clone());
                }
                InstructionValue::PropertyLoad {
                    property: PropertyLiteral::String(name),
                    ..
                } => {
                    out.insert(instr.lvalue.identifier.declaration_id, name.clone());
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(mapped) = out.get(&place.identifier.declaration_id) {
                        out.insert(instr.lvalue.identifier.declaration_id, mapped.clone());
                    }
                }
                InstructionValue::TypeCastExpression { value, .. } => {
                    if let Some(mapped) = out.get(&value.identifier.declaration_id) {
                        out.insert(instr.lvalue.identifier.declaration_id, mapped.clone());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn collect_load_global_declarations(func: &HIRFunction) -> HashMap<DeclarationId, String> {
    let mut out: HashMap<DeclarationId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
                match binding {
                    NonLocalBinding::Global { name } => {
                        out.insert(instr.lvalue.identifier.declaration_id, name.clone());
                    }
                    NonLocalBinding::ImportSpecifier {
                        module, imported, ..
                    } => {
                        out.insert(
                            instr.lvalue.identifier.declaration_id,
                            format!("{module}::{imported}"),
                        );
                    }
                    NonLocalBinding::ImportDefault { module, .. } => {
                        out.insert(
                            instr.lvalue.identifier.declaration_id,
                            format!("{module}::default"),
                        );
                    }
                    NonLocalBinding::ImportNamespace { module, .. } => {
                        out.insert(instr.lvalue.identifier.declaration_id, module.clone());
                    }
                    NonLocalBinding::ModuleLocal { .. } => {}
                }
            }
        }
    }
    out
}

fn collect_load_source_declarations(func: &HIRFunction) -> HashMap<DeclarationId, DeclarationId> {
    let mut out: HashMap<DeclarationId, DeclarationId> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    out.insert(
                        instr.lvalue.identifier.declaration_id,
                        place.identifier.declaration_id,
                    );
                }
                InstructionValue::TypeCastExpression { value, .. } => {
                    out.insert(
                        instr.lvalue.identifier.declaration_id,
                        value.identifier.declaration_id,
                    );
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if lvalue.kind != InstructionKind::Reassign {
                        out.insert(
                            lvalue.place.identifier.declaration_id,
                            value.identifier.declaration_id,
                        );
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn collect_jsx_returning_function_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    fn function_returns_jsx(func: &HIRFunction) -> bool {
        if type_maybe_contains_jsx(&func.returns.identifier.type_) {
            return true;
        }
        for (_, block) in &func.body.blocks {
            if let Terminal::Return { value, .. } = &block.terminal
                && type_maybe_contains_jsx(&value.identifier.type_)
            {
                return true;
            }
        }
        false
    }

    fn collect_recursive(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        if function_returns_jsx(&lowered_func.func) {
                            out.insert(instr.lvalue.identifier.declaration_id);
                        }
                        collect_recursive(&lowered_func.func, out);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut out = HashSet::new();
    collect_recursive(func, &mut out);
    out
}

fn collect_declaration_type_hints(func: &HIRFunction) -> HashMap<DeclarationId, Type> {
    let mut out: HashMap<DeclarationId, Type> = HashMap::new();
    let mut seed = |ident: &Identifier| {
        if matches!(ident.type_, Type::Poly | Type::TypeVar { .. }) {
            return;
        }
        out.entry(ident.declaration_id)
            .or_insert_with(|| ident.type_.clone());
    };

    for arg in &func.params {
        match arg {
            Argument::Place(place) | Argument::Spread(place) => seed(&place.identifier),
        }
    }
    for place in &func.context {
        seed(&place.identifier);
    }
    seed(&func.returns.identifier);

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            seed(&phi.place.identifier);
            for op in phi.operands.values() {
                seed(&op.identifier);
            }
        }
        for instr in &block.instructions {
            visitors::for_each_instruction_lvalue(instr, |place| seed(&place.identifier));
            visitors::for_each_instruction_operand(instr, |place| seed(&place.identifier));
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| seed(&place.identifier));
    }

    out
}

fn collect_local_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut out = HashSet::new();

    for arg in &func.params {
        match arg {
            Argument::Place(place) | Argument::Spread(place) => {
                out.insert(place.identifier.declaration_id);
            }
        }
    }

    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    out.insert(lvalue.place.identifier.declaration_id);
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign {
                        for_each_pattern_item(&lvalue.pattern, |place, _is_spread| {
                            out.insert(place.identifier.declaration_id);
                        });
                    }
                }
                _ => {}
            }
        }
    }

    out
}

fn collect_reassigned_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    let mut out = HashSet::new();
    fn walk(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
        for (_block_id, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                    | InstructionValue::StoreContext { lvalue, .. } => {
                        if lvalue.kind == InstructionKind::Reassign {
                            out.insert(lvalue.place.identifier.declaration_id);
                        }
                    }
                    InstructionValue::Destructure { lvalue, .. } => {
                        if lvalue.kind == InstructionKind::Reassign {
                            for_each_pattern_item(&lvalue.pattern, |place, _is_spread| {
                                out.insert(place.identifier.declaration_id);
                            });
                        }
                    }
                    InstructionValue::PostfixUpdate { lvalue, .. }
                    | InstructionValue::PrefixUpdate { lvalue, .. } => {
                        out.insert(lvalue.identifier.declaration_id);
                    }
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        walk(&lowered_func.func, out);
                    }
                    _ => {}
                }
            }
        }
    }
    walk(func, &mut out);

    out
}

// ---------------------------------------------------------------------------
// Parameter and context initialization
// ---------------------------------------------------------------------------

fn initialize_params_and_context(
    func: &HIRFunction,
    state: &mut InferenceState,
    is_function_expression: bool,
) {
    // Initialize context variables
    for ctx_place in &func.context {
        let vid = state.initialize_for_place(
            ctx_place.identifier.id,
            AbstractValue::new(ValueKind::Context, ValueReason::Other),
        );
        state.define(ctx_place.identifier.id, vid);
    }

    // Determine parameter kind
    let param_kind = if is_function_expression {
        AbstractValue::new(ValueKind::Mutable, ValueReason::Other)
    } else {
        AbstractValue::new(ValueKind::Frozen, ValueReason::ReactiveFunctionArgument)
    };

    if func.fn_type == ReactFunctionType::Component {
        // Component: first param is props, second is ref
        let params: Vec<&Argument> = func.params.iter().collect();
        if let Some(props) = params.first() {
            infer_param(props, state, &param_kind);
        }
        if let Some(ref_param) = params.get(1) {
            let place = match ref_param {
                Argument::Place(p) => p,
                Argument::Spread(p) => p,
            };
            let vid = state.initialize_for_place(
                place.identifier.id,
                AbstractValue::new(ValueKind::Mutable, ValueReason::Other),
            );
            state.define(place.identifier.id, vid);
        }
    } else {
        for param in &func.params {
            infer_param(param, state, &param_kind);
        }
    }
}

fn infer_param(param: &Argument, state: &mut InferenceState, param_kind: &AbstractValue) {
    let place = match param {
        Argument::Place(p) => p,
        Argument::Spread(p) => p,
    };
    let vid = state.initialize_for_place(place.identifier.id, param_kind.clone());
    state.define(place.identifier.id, vid);
}

fn collect_catch_handlers(
    func: &HIRFunction,
) -> (
    HashMap<BlockId, Place>,
    HashMap<BlockId, Place>,
    HashMap<BlockId, BlockId>,
) {
    let mut by_handler_block = HashMap::new();
    let mut by_try_block = HashMap::new();
    let mut handler_successor_by_try_block = HashMap::new();
    for (_, block) in &func.body.blocks {
        if let Terminal::Try {
            block: try_block,
            handler_binding: Some(binding),
            handler,
            ..
        } = &block.terminal
        {
            by_handler_block.insert(*handler, binding.clone());
            by_try_block.insert(*try_block, binding.clone());
            handler_successor_by_try_block.insert(*try_block, *handler);
        }
    }
    (
        by_handler_block,
        by_try_block,
        handler_successor_by_try_block,
    )
}

// ---------------------------------------------------------------------------
// Block inference
// ---------------------------------------------------------------------------

fn infer_block(
    ctx: &InferContext,
    state: &mut InferenceState,
    block_id: BlockId,
    block: &mut BasicBlock,
) {
    let debug_apply = std::env::var("DEBUG_APPLY_SIGNATURE").is_ok();
    if debug_apply {
        let terminal_kind = match &block.terminal {
            Terminal::Return { .. } => "Return",
            Terminal::Throw { .. } => "Throw",
            Terminal::If { .. } => "If",
            Terminal::Branch { .. } => "Branch",
            Terminal::Goto { .. } => "Goto",
            Terminal::Switch { .. } => "Switch",
            Terminal::Try { .. } => "Try",
            Terminal::MaybeThrow { .. } => "MaybeThrow",
            Terminal::Unsupported { .. } => "Unsupported",
            Terminal::Unreachable { .. } => "Unreachable",
            Terminal::For { .. } => "For",
            Terminal::ForOf { .. } => "ForOf",
            Terminal::ForIn { .. } => "ForIn",
            Terminal::While { .. } => "While",
            Terminal::DoWhile { .. } => "DoWhile",
            Terminal::Label { .. } => "Label",
            Terminal::Scope { .. } => "Scope",
            Terminal::PrunedScope { .. } => "PrunedScope",
            Terminal::Sequence { .. } => "Sequence",
            Terminal::Logical { .. } => "Logical",
            Terminal::Ternary { .. } => "Ternary",
            Terminal::Optional { .. } => "Optional",
        };
        eprintln!("[APPLY_SIG_BLOCK] terminal={}", terminal_kind);
    }

    fn summarize_effect(effect: &AliasingEffect) -> String {
        match effect {
            AliasingEffect::Assign { from, into } => {
                format!("Assign({}->{})", from.identifier.id.0, into.identifier.id.0)
            }
            AliasingEffect::Capture { from, into } => {
                format!(
                    "Capture({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::Alias { from, into } => {
                format!("Alias({}->{})", from.identifier.id.0, into.identifier.id.0)
            }
            AliasingEffect::MaybeAlias { from, into } => {
                format!(
                    "MaybeAlias({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::Create {
                into,
                value,
                reason,
            } => {
                format!("Create({};{:?};{:?})", into.identifier.id.0, value, reason)
            }
            AliasingEffect::CreateFrom { from, into } => {
                format!(
                    "CreateFrom({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::CreateFunction { into, .. } => {
                format!("CreateFunction({})", into.identifier.id.0)
            }
            AliasingEffect::Mutate { value, .. } => {
                format!("Mutate({})", value.identifier.id.0)
            }
            AliasingEffect::MutateConditionally { value } => {
                format!("MutateConditionally({})", value.identifier.id.0)
            }
            AliasingEffect::MutateTransitive { value } => {
                format!("MutateTransitive({})", value.identifier.id.0)
            }
            AliasingEffect::MutateTransitiveConditionally { value } => {
                format!("MutateTransitiveConditionally({})", value.identifier.id.0)
            }
            AliasingEffect::Freeze { value, .. } => {
                format!("Freeze({})", value.identifier.id.0)
            }
            AliasingEffect::ImmutableCapture { from, into } => {
                format!(
                    "ImmutableCapture({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::Apply { function, into, .. } => {
                format!(
                    "Apply({}->{})",
                    function.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::MutateFrozen { place, .. } => {
                format!("MutateFrozen({})", place.identifier.id.0)
            }
            AliasingEffect::MutateGlobal { place, .. } => {
                format!("MutateGlobal({})", place.identifier.id.0)
            }
            AliasingEffect::Impure { place, .. } => {
                format!("Impure({})", place.identifier.id.0)
            }
            AliasingEffect::Render { place } => {
                format!("Render({})", place.identifier.id.0)
            }
        }
    }

    // Process phis
    for phi in &block.phis {
        state.infer_phi(phi);
    }

    // Process instructions
    for instr in &mut block.instructions {
        let signature_effects = compute_signature_for_instruction(ctx, state, instr);
        let effects = apply_signature(ctx, state, &signature_effects, instr);
        if debug_apply {
            let kind = match &instr.value {
                InstructionValue::Primitive { .. } => "Primitive",
                InstructionValue::LoadLocal { .. } => "LoadLocal",
                InstructionValue::StoreLocal { .. } => "StoreLocal",
                InstructionValue::LoadGlobal { .. } => "LoadGlobal",
                InstructionValue::CallExpression { .. } => "CallExpression",
                InstructionValue::MethodCall { .. } => "MethodCall",
                InstructionValue::DeclareLocal { .. } => "DeclareLocal",
                _ => "Other",
            };
            if !effects.is_empty() {
                let rendered = effects
                    .iter()
                    .map(summarize_effect)
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "[APPLY_SIG_EFFECTS] instr#{} kind={} lvalue_id={} effects=[{}]",
                    instr.id.0, kind, instr.lvalue.identifier.id.0, rendered
                );
            }
        }
        instr.effects = if effects.is_empty() {
            None
        } else {
            Some(effects)
        };
    }

    // Process terminal effects.
    if let Terminal::Return { value, .. } = &mut block.terminal
        && !ctx.is_function_expression
    {
        state.freeze(value.identifier.id, ValueReason::JsxCaptured);
    }

    // Upstream behavior: values produced by calls in a maybe-throw block may flow
    // into the catch binding; model this by appending handler aliases.
    let catch_handler = match &block.terminal {
        Terminal::MaybeThrow { handler, .. } => ctx.catch_handlers_by_handler_block.get(handler),
        _ => ctx.catch_handlers_by_try_block.get(&block_id),
    };
    if let Some(handler_param) = catch_handler {
        let handler_id = handler_param.identifier.id;
        if debug_apply {
            eprintln!(
                "[APPLY_SIG_CATCH] block={} handler_id={} defined={}",
                block_id.0,
                handler_id.0,
                state.variables.contains_key(&handler_id)
            );
        }
        if state.variables.contains_key(&handler_id) {
            for instr in &mut block.instructions {
                if !matches!(
                    instr.value,
                    InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                ) {
                    continue;
                }

                state.append_alias(handler_id, instr.lvalue.identifier.id);
                let kind = state.kind(instr.lvalue.identifier.id).kind;
                if debug_apply {
                    eprintln!(
                        "[APPLY_SIG_CATCH] handler_id={} call_lvalue_id={} kind={:?}",
                        handler_id.0, instr.lvalue.identifier.id.0, kind
                    );
                }
                if matches!(kind, ValueKind::Mutable | ValueKind::Context) {
                    let has_handler_alias = instr.effects.as_ref().is_some_and(|effects| {
                        effects.iter().any(|effect| {
                            matches!(
                                effect,
                                AliasingEffect::Alias { from, into }
                                    if from.identifier.id == instr.lvalue.identifier.id
                                        && into.identifier.id == handler_id
                            )
                        })
                    });
                    if !has_handler_alias {
                        let effect = AliasingEffect::Alias {
                            from: instr.lvalue.clone(),
                            into: handler_param.clone(),
                        };
                        if debug_apply {
                            eprintln!(
                                "[APPLY_SIG_CATCH] append Alias({}->{}) to instr#{}",
                                instr.lvalue.identifier.id.0, handler_id.0, instr.id.0
                            );
                        }
                        if let Some(effects) = &mut instr.effects {
                            effects.push(effect);
                        } else {
                            instr.effects = Some(vec![effect]);
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Apply signature (effects to state)
// ---------------------------------------------------------------------------

fn apply_signature(
    ctx: &InferContext,
    state: &mut InferenceState,
    signature_effects: &[AliasingEffect],
    instr: &Instruction,
) -> Vec<AliasingEffect> {
    let mut effects = Vec::new();
    let mut initialized: HashSet<IdentifierId> = HashSet::new();

    // Eagerly validate local function effects against currently frozen context
    // values (upstream applySignature behavior for FunctionExpression/ObjectMethod).
    if let InstructionValue::FunctionExpression { lowered_func, .. }
    | InstructionValue::ObjectMethod { lowered_func, .. } = &instr.value
    {
        let context_ids: HashSet<IdentifierId> = lowered_func
            .func
            .context
            .iter()
            .map(|place| place.identifier.id)
            .collect();
        if let Some(aliasing_effects) = lowered_func.func.aliasing_effects.as_ref() {
            for effect in aliasing_effects {
                let (value, is_assign_current_property) = match effect {
                    AliasingEffect::Mutate { value, reason } => (
                        value,
                        reason.is_some_and(|r| r == MutationReason::AssignCurrentProperty),
                    ),
                    AliasingEffect::MutateTransitive { value } => (value, false),
                    _ => continue,
                };
                if !context_ids.contains(&value.identifier.id) {
                    continue;
                }
                let abs_val = state.kind(value.identifier.id);
                if abs_val.kind == ValueKind::Frozen {
                    effects.push(AliasingEffect::MutateFrozen {
                        place: value.clone(),
                        error: mutate_frozen_diagnostic(
                            ctx,
                            state,
                            value,
                            is_assign_current_property,
                        ),
                    });
                }
            }
        }
    }

    for effect in signature_effects {
        apply_effect(ctx, state, effect, &mut initialized, &mut effects);
    }

    // Ensure the lvalue is defined
    if !state.is_defined(instr.lvalue.identifier.id) {
        let vid = state.initialize_for_place(
            instr.lvalue.identifier.id,
            AbstractValue::new(ValueKind::Mutable, ValueReason::Other),
        );
        state.define(instr.lvalue.identifier.id, vid);
    }

    effects
}

fn apply_effect(
    ctx: &InferContext,
    state: &mut InferenceState,
    effect: &AliasingEffect,
    initialized: &mut HashSet<IdentifierId>,
    effects: &mut Vec<AliasingEffect>,
) {
    match effect {
        AliasingEffect::Freeze { value, reason } => {
            let did_freeze = state.freeze(value.identifier.id, *reason);
            if did_freeze {
                effects.push(effect.clone());
            }
        }

        AliasingEffect::Create {
            into,
            value,
            reason,
        } => {
            initialized.insert(into.identifier.id);

            let abs_val = AbstractValue::new(*value, *reason);
            let vid = state.initialize_for_place(into.identifier.id, abs_val);
            state.define(into.identifier.id, vid);
            effects.push(effect.clone());
        }

        AliasingEffect::CreateFrom { from, into } => {
            initialized.insert(into.identifier.id);

            let from_value = state.kind(from.identifier.id);
            let vid = state.initialize_for_place(into.identifier.id, from_value.clone());
            state.define(into.identifier.id, vid);

            match from_value.kind {
                ValueKind::Primitive | ValueKind::Global => {
                    effects.push(AliasingEffect::Create {
                        value: from_value.kind,
                        into: into.clone(),
                        reason: from_value.first_reason(),
                    });
                }
                ValueKind::Frozen => {
                    effects.push(AliasingEffect::Create {
                        value: from_value.kind,
                        into: into.clone(),
                        reason: from_value.first_reason(),
                    });
                    // Also emit ImmutableCapture
                    apply_effect(
                        ctx,
                        state,
                        &AliasingEffect::ImmutableCapture {
                            from: from.clone(),
                            into: into.clone(),
                        },
                        initialized,
                        effects,
                    );
                }
                _ => {
                    effects.push(effect.clone());
                }
            }
        }

        AliasingEffect::CreateFunction {
            captures,
            into,
            signature,
            context,
        } => {
            // Ensure hoisted context declarations exist before they are frozen via
            // transitive function freezing (e.g. useEffect(() => setState(...))).
            for operand in context {
                let decl = operand.identifier.declaration_id;
                if ctx.hoisted_context_declarations.contains(&decl)
                    && !state.is_defined(operand.identifier.id)
                {
                    let vid = state.initialize_for_place(
                        operand.identifier.id,
                        AbstractValue::new(ValueKind::Mutable, ValueReason::Other),
                    );
                    state.define(operand.identifier.id, vid);
                }
            }

            initialized.insert(into.identifier.id);
            effects.push(effect.clone());

            // Determine if the function should be considered mutable
            let has_mutable_captures = captures.iter().any(|cap| {
                let k = state.kind(cap.identifier.id);
                matches!(k.kind, ValueKind::Context | ValueKind::Mutable)
            });
            let has_tracked_side_effects = signature.as_ref().is_some_and(|sig| {
                sig.effects.iter().any(|effect| {
                    matches!(
                        effect,
                        AliasingEffect::MutateFrozen { .. }
                            | AliasingEffect::MutateGlobal { .. }
                            | AliasingEffect::Impure { .. }
                    )
                })
            });
            let captures_ref = context
                .iter()
                .any(|operand| is_ref_or_ref_value(&operand.identifier));
            let func_kind = if has_mutable_captures || has_tracked_side_effects || captures_ref {
                ValueKind::Mutable
            } else {
                ValueKind::Frozen
            };
            let vid = state.initialize_for_place(
                into.identifier.id,
                AbstractValue::with_reasons(func_kind, HashSet::new()),
            );
            state.define(into.identifier.id, vid);
            state.set_function_contexts(vid, context.clone());
            if let Some(sig) = signature {
                state.set_function_signature(
                    vid,
                    StoredFunctionSignature {
                        signature: sig.clone(),
                        context: context.clone(),
                    },
                );
            }

            // Apply Capture for each captured place
            for cap in captures {
                apply_effect(
                    ctx,
                    state,
                    &AliasingEffect::Capture {
                        from: cap.clone(),
                        into: into.clone(),
                    },
                    initialized,
                    effects,
                );
            }
        }

        AliasingEffect::ImmutableCapture { from, .. } => {
            let kind = state.kind(from.identifier.id);
            match kind.kind {
                ValueKind::Global | ValueKind::Primitive => {
                    // no-op: don't need to track data flow for copy types
                }
                _ => {
                    effects.push(effect.clone());
                }
            }
        }

        AliasingEffect::Assign { from, into } => {
            initialized.insert(into.identifier.id);

            let from_value = state.kind(from.identifier.id);
            match from_value.kind {
                ValueKind::Frozen => {
                    // Emit ImmutableCapture
                    apply_effect(
                        ctx,
                        state,
                        &AliasingEffect::ImmutableCapture {
                            from: from.clone(),
                            into: into.clone(),
                        },
                        initialized,
                        effects,
                    );
                    let vid = state.initialize_for_place(
                        into.identifier.id,
                        AbstractValue::with_reasons(from_value.kind, from_value.reasons.clone()),
                    );
                    state.define(into.identifier.id, vid);
                }
                ValueKind::Global | ValueKind::Primitive => {
                    let vid = state.initialize_for_place(
                        into.identifier.id,
                        AbstractValue::with_reasons(from_value.kind, from_value.reasons.clone()),
                    );
                    state.define(into.identifier.id, vid);
                }
                _ => {
                    // Point-to aliasing
                    state.assign(into.identifier.id, from.identifier.id);
                    effects.push(effect.clone());
                }
            }
        }

        AliasingEffect::Capture { from, into }
        | AliasingEffect::Alias { from, into }
        | AliasingEffect::MaybeAlias { from, into } => {
            let into_kind = state.kind(into.identifier.id);
            let mut destination_type: Option<DestinationType> = None;
            match into_kind.kind {
                ValueKind::Context => {
                    destination_type = Some(DestinationType::Context);
                }
                ValueKind::Mutable | ValueKind::MaybeFrozen => {
                    destination_type = Some(DestinationType::Mutable);
                }
                _ => {}
            }

            let from_kind = state.kind(from.identifier.id);
            let mut source_type: Option<SourceType> = None;
            match from_kind.kind {
                ValueKind::Context => {
                    source_type = Some(SourceType::Context);
                }
                ValueKind::Global | ValueKind::Primitive => {
                    // no source type: copy semantics
                }
                ValueKind::Frozen => {
                    source_type = Some(SourceType::Frozen);
                }
                _ => {
                    source_type = Some(SourceType::Mutable);
                }
            }

            if source_type == Some(SourceType::Frozen) {
                apply_effect(
                    ctx,
                    state,
                    &AliasingEffect::ImmutableCapture {
                        from: from.clone(),
                        into: into.clone(),
                    },
                    initialized,
                    effects,
                );
            } else if (source_type == Some(SourceType::Mutable)
                && destination_type == Some(DestinationType::Mutable))
                || matches!(effect, AliasingEffect::MaybeAlias { .. })
            {
                effects.push(effect.clone());
            } else if (source_type == Some(SourceType::Context) && destination_type.is_some())
                || (source_type == Some(SourceType::Mutable)
                    && destination_type == Some(DestinationType::Context))
            {
                apply_effect(
                    ctx,
                    state,
                    &AliasingEffect::MaybeAlias {
                        from: from.clone(),
                        into: into.clone(),
                    },
                    initialized,
                    effects,
                );
            }
        }

        AliasingEffect::Apply {
            receiver,
            function,
            mutates_function,
            args,
            into,
            signature,
            loc,
        } => {
            let debug_apply = std::env::var("DEBUG_APPLY_SIGNATURE").is_ok();
            if !state.is_defined(function.identifier.id) {
                if debug_apply || std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                    eprintln!(
                        "[BAILOUT_REASON] infer-mutation undefined-callee id={} decl={} name={:?}",
                        function.identifier.id.0,
                        function.identifier.declaration_id.0,
                        function.identifier.name
                    );
                }
                effects.push(AliasingEffect::Impure {
                    place: function.clone(),
                    error: uninitialized_value_kind_diagnostic(function),
                });
                return;
            }
            if let Some(global_name) = ctx
                .global_name_by_declaration
                .get(&function.identifier.declaration_id)
                && let Some(global_effects) =
                    compute_known_global_aliasing_effects(global_name, into, args)
            {
                if debug_apply {
                    eprintln!(
                        "[APPLY_SIG_GLOBAL_ALIASING] name={} function_id={} decl={} into_id={} effect_count={}",
                        global_name,
                        function.identifier.id.0,
                        function.identifier.declaration_id.0,
                        into.identifier.id.0,
                        global_effects.len()
                    );
                }
                for global_effect in &global_effects {
                    apply_effect(ctx, state, global_effect, initialized, effects);
                }
                return;
            }
            if let Some(function_signature) = state
                .local_function_signature(function.identifier.id)
                .cloned()
            {
                if debug_apply {
                    let mut summary: Vec<String> = Vec::new();
                    for effect in &function_signature.signature.effects {
                        let label = match effect {
                            AliasingEffect::Mutate { value, .. } => {
                                format!("Mutate({})", value.identifier.id.0)
                            }
                            AliasingEffect::MutateConditionally { value } => {
                                format!("MutateConditionally({})", value.identifier.id.0)
                            }
                            AliasingEffect::MutateTransitive { value } => {
                                format!("MutateTransitive({})", value.identifier.id.0)
                            }
                            AliasingEffect::MutateTransitiveConditionally { value } => {
                                format!("MutateTransitiveConditionally({})", value.identifier.id.0)
                            }
                            AliasingEffect::Capture { from, into } => {
                                format!(
                                    "Capture({}->{})",
                                    from.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::Alias { from, into } => {
                                format!("Alias({}->{})", from.identifier.id.0, into.identifier.id.0)
                            }
                            AliasingEffect::MaybeAlias { from, into } => {
                                format!(
                                    "MaybeAlias({}->{})",
                                    from.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::Create { into, .. } => {
                                format!("Create({})", into.identifier.id.0)
                            }
                            AliasingEffect::CreateFrom { from, into } => {
                                format!(
                                    "CreateFrom({}->{})",
                                    from.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::Assign { from, into } => {
                                format!(
                                    "Assign({}->{})",
                                    from.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::Freeze { value, .. } => {
                                format!("Freeze({})", value.identifier.id.0)
                            }
                            AliasingEffect::ImmutableCapture { from, into } => {
                                format!(
                                    "ImmutableCapture({}->{})",
                                    from.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::Apply { function, into, .. } => {
                                format!(
                                    "Apply({}->{})",
                                    function.identifier.id.0, into.identifier.id.0
                                )
                            }
                            AliasingEffect::CreateFunction { into, .. } => {
                                format!("CreateFunction({})", into.identifier.id.0)
                            }
                            AliasingEffect::MutateFrozen { place, .. } => {
                                format!("MutateFrozen({})", place.identifier.id.0)
                            }
                            AliasingEffect::MutateGlobal { place, .. } => {
                                format!("MutateGlobal({})", place.identifier.id.0)
                            }
                            AliasingEffect::Impure { place, .. } => {
                                format!("Impure({})", place.identifier.id.0)
                            }
                            AliasingEffect::Render { place } => {
                                format!("Render({})", place.identifier.id.0)
                            }
                        };
                        summary.push(label);
                    }
                    eprintln!(
                        "[APPLY_SIG] local-signature function_id={} decl={} name={:?} receiver_id={} into_id={} signature_effects=[{}]",
                        function.identifier.id.0,
                        function.identifier.declaration_id.0,
                        function.identifier.name,
                        receiver.identifier.id.0,
                        into.identifier.id.0,
                        summary.join(", ")
                    );
                }
                if let Some(signature_effects) = compute_effects_for_signature(
                    &function_signature.signature,
                    into,
                    receiver,
                    args,
                    &function_signature.context,
                    loc,
                ) {
                    apply_effect(
                        ctx,
                        state,
                        &AliasingEffect::MutateTransitiveConditionally {
                            value: function.clone(),
                        },
                        initialized,
                        effects,
                    );
                    for signature_effect in &signature_effects {
                        apply_effect(ctx, state, signature_effect, initialized, effects);
                    }
                    return;
                }
            }
            if debug_apply {
                eprintln!(
                    "[APPLY_SIG] fallback function_id={} decl={} name={:?} receiver_id={} into_id={} has_builtin_sig={}",
                    function.identifier.id.0,
                    function.identifier.declaration_id.0,
                    function.identifier.name,
                    receiver.identifier.id.0,
                    into.identifier.id.0,
                    signature.is_some()
                );
            }

            // Try to use the signature if available
            if let Some(sig) = signature {
                let legacy_effects = compute_effects_for_legacy_signature(
                    ctx, state, sig, into, receiver, args, loc,
                );
                for legacy_effect in &legacy_effects {
                    apply_effect(ctx, state, legacy_effect, initialized, effects);
                }
            } else {
                // Default: no signature
                // Create the return value as mutable
                apply_effect(
                    ctx,
                    state,
                    &AliasingEffect::Create {
                        into: *into.clone(),
                        value: ValueKind::Mutable,
                        reason: ValueReason::Other,
                    },
                    initialized,
                    effects,
                );

                // Conditionally mutate all operands and capture into result.
                let mut all_operands: Vec<(Place, bool)> =
                    vec![(receiver.clone(), false), (function.clone(), false)];
                for arg in args {
                    match arg {
                        ApplyArg::Hole => {}
                        ApplyArg::Place(place) => {
                            all_operands.push((place.clone(), false));
                        }
                        ApplyArg::Spread(place) => {
                            all_operands.push((place.clone(), true));
                        }
                    }
                }
                for (operand, is_spread) in &all_operands {
                    // Mirror upstream `operand !== effect.function` semantics.
                    // Cloned Rust `Place`s lose TS object identity, so compare SSA ids.
                    if operand.identifier.id != function.identifier.id || *mutates_function {
                        apply_effect(
                            ctx,
                            state,
                            &AliasingEffect::MutateTransitiveConditionally {
                                value: operand.clone(),
                            },
                            initialized,
                            effects,
                        );
                    }
                    if *is_spread {
                        if let Some(mutate_iterator) = conditionally_mutate_iterator(operand) {
                            apply_effect(ctx, state, &mutate_iterator, initialized, effects);
                        }
                    }

                    apply_effect(
                        ctx,
                        state,
                        &AliasingEffect::MaybeAlias {
                            from: operand.clone(),
                            into: *into.clone(),
                        },
                        initialized,
                        effects,
                    );

                    for (other, _) in &all_operands {
                        if other.identifier.id == operand.identifier.id {
                            continue;
                        }
                        apply_effect(
                            ctx,
                            state,
                            &AliasingEffect::Capture {
                                from: operand.clone(),
                                into: other.clone(),
                            },
                            initialized,
                            effects,
                        );
                    }
                }
            }
        }

        AliasingEffect::Mutate { value, reason } => {
            let result = state.mutate(MutationVariant::Mutate, value);
            match result {
                MutationResult::Mutate => effects.push(effect.clone()),
                MutationResult::MutateRef => {}
                MutationResult::MutateFrozen => {
                    let abs_val = state.kind(value.identifier.id);
                    if should_allow_maybe_frozen_jsx_mutation(value, &abs_val) {
                        return;
                    }
                    if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                        eprintln!(
                            "[BAILOUT_REASON] infer-mutation mutate-frozen place_id={} decl={} name={:?} kind={:?} reasons={:?}",
                            value.identifier.id.0,
                            value.identifier.declaration_id.0,
                            value.identifier.name,
                            abs_val.kind,
                            abs_val.reasons
                        );
                    }
                    effects.push(AliasingEffect::MutateFrozen {
                        place: value.clone(),
                        error: mutate_frozen_diagnostic(
                            ctx,
                            state,
                            value,
                            reason.is_some_and(|r| r == MutationReason::AssignCurrentProperty),
                        ),
                    });
                }
                MutationResult::MutateGlobal => {
                    effects.push(AliasingEffect::MutateGlobal {
                        place: value.clone(),
                        error: CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: "Cannot mutate a global value".to_string(),
                        },
                    });
                }
                MutationResult::None => {}
            }
        }

        AliasingEffect::MutateConditionally { value } => {
            let result = state.mutate(MutationVariant::MutateConditionally, value);
            if result == MutationResult::Mutate {
                effects.push(effect.clone());
            }
        }

        AliasingEffect::MutateTransitive { value } => {
            let result = state.mutate(MutationVariant::MutateTransitive, value);
            match result {
                MutationResult::Mutate => effects.push(effect.clone()),
                MutationResult::MutateRef => {}
                MutationResult::MutateFrozen => {
                    let abs_val = state.kind(value.identifier.id);
                    if should_allow_maybe_frozen_jsx_mutation(value, &abs_val) {
                        return;
                    }
                    if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                        eprintln!(
                            "[BAILOUT_REASON] infer-mutation mutate-transitive-frozen place_id={} decl={} name={:?} kind={:?} reasons={:?}",
                            value.identifier.id.0,
                            value.identifier.declaration_id.0,
                            value.identifier.name,
                            abs_val.kind,
                            abs_val.reasons
                        );
                    }
                    effects.push(AliasingEffect::MutateFrozen {
                        place: value.clone(),
                        error: mutate_frozen_diagnostic(ctx, state, value, false),
                    });
                }
                MutationResult::MutateGlobal => {
                    effects.push(AliasingEffect::MutateGlobal {
                        place: value.clone(),
                        error: CompilerDiagnostic {
                            severity: DiagnosticSeverity::InvalidReact,
                            message: "Cannot mutate a global value".to_string(),
                        },
                    });
                }
                MutationResult::None => {}
            }
        }

        AliasingEffect::MutateTransitiveConditionally { value } => {
            let result = state.mutate(MutationVariant::MutateTransitiveConditionally, value);
            if result == MutationResult::Mutate {
                effects.push(effect.clone());
            }
        }

        AliasingEffect::Impure { .. }
        | AliasingEffect::Render { .. }
        | AliasingEffect::MutateFrozen { .. }
        | AliasingEffect::MutateGlobal { .. } => {
            effects.push(effect.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers for Apply
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestinationType {
    Context,
    Mutable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceType {
    Context,
    Frozen,
    Mutable,
}

fn conditionally_mutate_iterator(place: &Place) -> Option<AliasingEffect> {
    if !(is_array_type(&place.identifier)
        || is_set_type(&place.identifier)
        || is_map_type(&place.identifier))
    {
        return Some(AliasingEffect::MutateTransitiveConditionally {
            value: place.clone(),
        });
    }
    None
}

fn is_ref_or_ref_value(identifier: &Identifier) -> bool {
    matches!(
        &identifier.type_,
        Type::Object { shape_id: Some(s) } if s == "BuiltInUseRefId" || s == "BuiltInRefValue"
    )
}

fn build_signature_from_function_expression(
    lowered_func: &LoweredFunction,
) -> Option<AliasingSignature> {
    let function_effects = lowered_func.func.aliasing_effects.clone()?;

    let mut params: Vec<IdentifierId> = Vec::new();
    let mut rest: Option<IdentifierId> = None;
    for param in &lowered_func.func.params {
        match param {
            Argument::Place(p) => params.push(p.identifier.id),
            Argument::Spread(p) => rest = Some(p.identifier.id),
        }
    }

    let returns = lowered_func.func.returns.identifier.id;
    let rest = Some(rest.unwrap_or_else(|| {
        fresh_signature_identifier_id(
            &params,
            returns,
            &function_effects,
            &lowered_func.func.context,
        )
    }));

    Some(AliasingSignature {
        receiver: make_identifier_id(0),
        params,
        rest,
        returns,
        effects: function_effects,
        temporaries: Vec::new(),
    })
}

pub(crate) fn build_signature_for_lowered_function(
    lowered_func: &LoweredFunction,
) -> Option<AliasingSignature> {
    build_signature_from_function_expression(lowered_func)
}

fn lowered_function_has_shadowed_named_local(
    lowered_func: &LoweredFunction,
    function_name: &str,
) -> bool {
    for (_block_id, block) in &lowered_func.func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign
                        && place_matches_named_identifier(&lvalue.place, function_name)
                    {
                        return true;
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        continue;
                    }
                    let mut matched = false;
                    for_each_pattern_item(&lvalue.pattern, |place, _is_spread| {
                        if place_matches_named_identifier(place, function_name) {
                            matched = true;
                        }
                    });
                    if matched {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn place_matches_named_identifier(place: &Place, expected_name: &str) -> bool {
    match &place.identifier.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            is_same_or_renamed_identifier(name, expected_name)
        }
        _ => false,
    }
}

fn is_same_or_renamed_identifier(actual_name: &str, expected_name: &str) -> bool {
    if actual_name == expected_name {
        return true;
    }
    let Some(suffix) = actual_name.strip_prefix(expected_name) else {
        return false;
    };
    let Some(rest) = suffix.strip_prefix('_') else {
        return false;
    };
    let mut chars = rest.chars().peekable();
    let mut has_digits = false;
    while let Some(ch) = chars.peek().copied() {
        if ch.is_ascii_digit() {
            has_digits = true;
            chars.next();
        } else {
            break;
        }
    }
    if !has_digits {
        return false;
    }
    if chars.peek().is_none() {
        return true;
    }
    if chars.next() != Some('$') {
        return false;
    }
    let mut has_ssa_digits = false;
    for ch in chars {
        if !ch.is_ascii_digit() {
            return false;
        }
        has_ssa_digits = true;
    }
    has_ssa_digits
}

fn fresh_signature_identifier_id(
    params: &[IdentifierId],
    returns: IdentifierId,
    effects: &[AliasingEffect],
    context: &[Place],
) -> IdentifierId {
    let mut used = HashSet::new();
    used.insert(make_identifier_id(0));
    used.extend(params.iter().copied());
    used.insert(returns);
    for operand in context {
        used.insert(operand.identifier.id);
    }
    for effect in effects {
        collect_effect_identifier_ids(effect, &mut used);
    }

    let mut candidate = 0u32;
    while used.contains(&IdentifierId(candidate)) {
        if candidate == u32::MAX {
            break;
        }
        candidate += 1;
    }
    IdentifierId(candidate)
}

fn collect_effect_identifier_ids(effect: &AliasingEffect, into: &mut HashSet<IdentifierId>) {
    match effect {
        AliasingEffect::Freeze { value, .. }
        | AliasingEffect::Mutate { value, .. }
        | AliasingEffect::MutateConditionally { value }
        | AliasingEffect::MutateTransitive { value }
        | AliasingEffect::MutateTransitiveConditionally { value } => {
            into.insert(value.identifier.id);
        }
        AliasingEffect::Capture { from, into: to }
        | AliasingEffect::Alias { from, into: to }
        | AliasingEffect::MaybeAlias { from, into: to }
        | AliasingEffect::Assign { from, into: to }
        | AliasingEffect::CreateFrom { from, into: to }
        | AliasingEffect::ImmutableCapture { from, into: to } => {
            into.insert(from.identifier.id);
            into.insert(to.identifier.id);
        }
        AliasingEffect::Create { into: to, .. } => {
            into.insert(to.identifier.id);
        }
        AliasingEffect::Apply {
            receiver,
            function,
            args,
            into: to,
            ..
        } => {
            into.insert(receiver.identifier.id);
            into.insert(function.identifier.id);
            into.insert(to.identifier.id);
            for arg in args {
                match arg {
                    ApplyArg::Place(p) | ApplyArg::Spread(p) => {
                        into.insert(p.identifier.id);
                    }
                    ApplyArg::Hole => {}
                }
            }
        }
        AliasingEffect::CreateFunction {
            captures,
            into: to,
            signature,
            context,
        } => {
            into.insert(to.identifier.id);
            for capture in captures {
                into.insert(capture.identifier.id);
            }
            for operand in context {
                into.insert(operand.identifier.id);
            }
            if let Some(signature) = signature {
                into.insert(signature.receiver);
                into.insert(signature.returns);
                into.extend(signature.params.iter().copied());
                if let Some(rest) = signature.rest {
                    into.insert(rest);
                }
                for temporary in &signature.temporaries {
                    into.insert(temporary.identifier.id);
                }
                for nested in &signature.effects {
                    collect_effect_identifier_ids(nested, into);
                }
            }
        }
        AliasingEffect::MutateFrozen { place, .. }
        | AliasingEffect::MutateGlobal { place, .. }
        | AliasingEffect::Impure { place, .. }
        | AliasingEffect::Render { place } => {
            into.insert(place.identifier.id);
        }
    }
}

fn substitution_values_or_self(
    substitutions: &HashMap<IdentifierId, Vec<Place>>,
    place: &Place,
) -> Vec<Place> {
    substitutions
        .get(&place.identifier.id)
        .cloned()
        .unwrap_or_else(|| vec![place.clone()])
}

fn single_substitution_or_self(
    substitutions: &HashMap<IdentifierId, Vec<Place>>,
    place: &Place,
) -> Option<Place> {
    match substitutions.get(&place.identifier.id) {
        Some(values) => {
            if values.len() != 1 {
                return None;
            }
            Some(values[0].clone())
        }
        None => Some(place.clone()),
    }
}

fn compute_effects_for_signature(
    signature: &AliasingSignature,
    lvalue: &Place,
    receiver: &Place,
    args: &[ApplyArg],
    context: &[Place],
    loc: &SourceLocation,
) -> Option<Vec<AliasingEffect>> {
    // Not enough args.
    if signature.params.len() > args.len() {
        return None;
    }
    // Too many args and there is no rest param to hold them.
    if args.len() > signature.params.len() && signature.rest.is_none() {
        return None;
    }

    let mut mutable_spreads: HashSet<IdentifierId> = HashSet::new();
    let mut substitutions: HashMap<IdentifierId, Vec<Place>> = HashMap::new();
    substitutions.insert(signature.receiver, vec![receiver.clone()]);
    substitutions.insert(signature.returns, vec![lvalue.clone()]);

    for (i, arg) in args.iter().enumerate() {
        match arg {
            ApplyArg::Hole => {}
            ApplyArg::Place(place) => {
                if i >= signature.params.len() {
                    let rest = signature.rest?;
                    substitutions.entry(rest).or_default().push(place.clone());
                } else {
                    substitutions.insert(signature.params[i], vec![place.clone()]);
                }
            }
            ApplyArg::Spread(place) => {
                let rest = signature.rest?;
                substitutions.entry(rest).or_default().push(place.clone());
                if conditionally_mutate_iterator(place).is_some() {
                    mutable_spreads.insert(place.identifier.id);
                }
            }
        }
    }

    for operand in context {
        substitutions.insert(operand.identifier.id, vec![operand.clone()]);
    }

    // Signature-local temporaries may appear in nested effects; keep them
    // addressable so substitution does not drop those effects.
    for temporary in &signature.temporaries {
        substitutions
            .entry(temporary.identifier.id)
            .or_insert_with(|| vec![temporary.clone()]);
    }

    let mut effects = Vec::new();

    for effect in &signature.effects {
        match effect {
            AliasingEffect::MaybeAlias { from, into }
            | AliasingEffect::Assign { from, into }
            | AliasingEffect::ImmutableCapture { from, into }
            | AliasingEffect::Alias { from, into }
            | AliasingEffect::CreateFrom { from, into }
            | AliasingEffect::Capture { from, into } => {
                let from_values = substitutions
                    .get(&from.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, from));
                let into_values = substitutions
                    .get(&into.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, into));
                for from_value in &from_values {
                    for into_value in &into_values {
                        match effect {
                            AliasingEffect::MaybeAlias { .. } => {
                                effects.push(AliasingEffect::MaybeAlias {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            AliasingEffect::Assign { .. } => {
                                effects.push(AliasingEffect::Assign {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            AliasingEffect::ImmutableCapture { .. } => {
                                effects.push(AliasingEffect::ImmutableCapture {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            AliasingEffect::Alias { .. } => {
                                effects.push(AliasingEffect::Alias {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            AliasingEffect::CreateFrom { .. } => {
                                effects.push(AliasingEffect::CreateFrom {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            AliasingEffect::Capture { .. } => {
                                effects.push(AliasingEffect::Capture {
                                    from: from_value.clone(),
                                    into: into_value.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            AliasingEffect::Impure { place, error } => {
                let values = substitutions
                    .get(&place.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, place));
                for value in values {
                    effects.push(AliasingEffect::Impure {
                        place: value,
                        error: error.clone(),
                    });
                }
            }
            AliasingEffect::MutateFrozen { place, error } => {
                let values = substitutions
                    .get(&place.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, place));
                for value in values {
                    effects.push(AliasingEffect::MutateFrozen {
                        place: value,
                        error: error.clone(),
                    });
                }
            }
            AliasingEffect::MutateGlobal { place, error } => {
                let values = substitutions
                    .get(&place.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, place));
                for value in values {
                    effects.push(AliasingEffect::MutateGlobal {
                        place: value,
                        error: error.clone(),
                    });
                }
            }
            AliasingEffect::Render { place } => {
                let values = substitutions
                    .get(&place.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, place));
                for value in values {
                    effects.push(AliasingEffect::Render { place: value });
                }
            }
            AliasingEffect::Mutate { value, reason } => {
                let values = substitutions
                    .get(&value.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, value));
                for value in values {
                    effects.push(AliasingEffect::Mutate {
                        value,
                        reason: *reason,
                    });
                }
            }
            AliasingEffect::MutateTransitive { value } => {
                let values = substitutions
                    .get(&value.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, value));
                for value in values {
                    effects.push(AliasingEffect::MutateTransitive { value });
                }
            }
            AliasingEffect::MutateTransitiveConditionally { value } => {
                let values = substitutions
                    .get(&value.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, value));
                for value in values {
                    effects.push(AliasingEffect::MutateTransitiveConditionally { value });
                }
            }
            AliasingEffect::MutateConditionally { value } => {
                let values = substitutions
                    .get(&value.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, value));
                for value in values {
                    effects.push(AliasingEffect::MutateConditionally { value });
                }
            }
            AliasingEffect::Freeze { value, reason } => {
                let values = substitutions
                    .get(&value.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, value));
                for value in values {
                    if mutable_spreads.contains(&value.identifier.id) {
                        return None;
                    }
                    effects.push(AliasingEffect::Freeze {
                        value,
                        reason: *reason,
                    });
                }
            }
            AliasingEffect::Create {
                into,
                value,
                reason,
            } => {
                let into_values = substitutions
                    .get(&into.identifier.id)
                    .cloned()
                    .unwrap_or_else(|| substitution_values_or_self(&substitutions, into));
                for into_value in into_values {
                    effects.push(AliasingEffect::Create {
                        into: into_value,
                        value: *value,
                        reason: *reason,
                    });
                }
            }
            AliasingEffect::Apply {
                receiver,
                function,
                mutates_function,
                args,
                into,
                signature,
                ..
            } => {
                let apply_receiver = single_substitution_or_self(&substitutions, receiver)?;
                let apply_function = single_substitution_or_self(&substitutions, function)?;
                let apply_into = single_substitution_or_self(&substitutions, into)?;

                let mut apply_args = Vec::with_capacity(args.len());
                for arg in args {
                    match arg {
                        ApplyArg::Hole => apply_args.push(ApplyArg::Hole),
                        ApplyArg::Place(place) => {
                            let apply_arg = single_substitution_or_self(&substitutions, place)?;
                            apply_args.push(ApplyArg::Place(apply_arg));
                        }
                        ApplyArg::Spread(place) => {
                            let apply_arg = single_substitution_or_self(&substitutions, place)?;
                            apply_args.push(ApplyArg::Spread(apply_arg));
                        }
                    }
                }

                effects.push(AliasingEffect::Apply {
                    receiver: apply_receiver,
                    function: apply_function,
                    mutates_function: *mutates_function,
                    args: apply_args,
                    into: Box::new(apply_into),
                    signature: signature.clone(),
                    loc: loc.clone(),
                });
            }
            AliasingEffect::CreateFunction { .. } => {
                return None;
            }
        }
    }

    Some(effects)
}

// ---------------------------------------------------------------------------
// Compute signature for instruction
// ---------------------------------------------------------------------------

fn is_hoisted_setter_like_capture(ctx: &InferContext, place: &Place) -> bool {
    if !ctx
        .hoisted_context_declarations
        .contains(&place.identifier.declaration_id)
    {
        return false;
    }
    matches!(
        &place.identifier.type_,
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } if matches!(
            shape_id.as_str(),
            "BuiltInSetState" | "BuiltInDispatch" | "BuiltInSetActionState"
        )
    )
}

/// Computes the candidate effects for an instruction based purely on its syntax
/// and types. This is cached (first visit) in the upstream implementation.
fn compute_signature_for_instruction(
    ctx: &InferContext,
    state: &InferenceState,
    instr: &Instruction,
) -> Vec<AliasingEffect> {
    let lvalue = &instr.lvalue;
    let value = &instr.value;
    let mut effects = Vec::new();

    match value {
        InstructionValue::ArrayExpression { elements, .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            });
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) => {
                        effects.push(AliasingEffect::Capture {
                            from: p.clone(),
                            into: lvalue.clone(),
                        });
                    }
                    ArrayElement::Spread(p) => {
                        if let Some(mutate_iterator) = conditionally_mutate_iterator(p) {
                            effects.push(mutate_iterator);
                        }
                        effects.push(AliasingEffect::Capture {
                            from: p.clone(),
                            into: lvalue.clone(),
                        });
                    }
                    ArrayElement::Hole => {}
                }
            }
        }

        InstructionValue::ObjectExpression { properties, .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            });
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        effects.push(AliasingEffect::Capture {
                            from: p.place.clone(),
                            into: lvalue.clone(),
                        });
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        effects.push(AliasingEffect::Capture {
                            from: p.clone(),
                            into: lvalue.clone(),
                        });
                    }
                }
            }
        }

        InstructionValue::Await { value: awaited, .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            });
            effects.push(AliasingEffect::MutateTransitiveConditionally {
                value: awaited.clone(),
            });
            effects.push(AliasingEffect::Capture {
                from: awaited.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::CallExpression { callee, args, .. } => {
            let callee_global_name = resolve_global_name(ctx, callee);
            let signature = get_function_call_signature(&callee.identifier.type_)
                .or_else(|| {
                    if state.enable_preserve_existing_memoization_guarantees
                        && callee_global_name.as_deref() == Some("shared-runtime::identity")
                    {
                        Some(FunctionSignature {
                            positional_params: vec![Effect::Read],
                            rest_param: Some(Effect::Read),
                            return_type: ReturnType::Poly,
                            return_value_kind: ValueKind::Mutable,
                            callee_effect: Effect::Read,
                            ..Default::default()
                        })
                    } else {
                        get_known_global_call_signature(
                            callee_global_name.as_deref(),
                            &lvalue.identifier,
                        )
                    }
                })
                .or_else(|| {
                    callee_global_name
                        .as_deref()
                        .and_then(get_known_global_function_signature)
                });
            let apply_args = args
                .iter()
                .map(|a| match a {
                    Argument::Place(p) => ApplyArg::Place(p.clone()),
                    Argument::Spread(p) => ApplyArg::Spread(p.clone()),
                })
                .collect();
            effects.push(AliasingEffect::Apply {
                receiver: callee.clone(),
                function: callee.clone(),
                mutates_function: true,
                args: apply_args,
                into: Box::new(lvalue.clone()),
                signature,
                loc: value.loc().clone(),
            });
        }

        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            let debug_apply = std::env::var("DEBUG_APPLY_SIGNATURE").is_ok();
            let method_name = resolve_method_name(ctx, property);
            let mut signature = get_known_method_signature(
                ctx,
                receiver,
                method_name.as_deref(),
                &lvalue.identifier,
            )
            .or_else(|| get_function_call_signature(&property.identifier.type_));
            if signature.is_none()
                && method_name
                    .as_deref()
                    .is_some_and(Environment::is_hook_name)
            {
                let shape_id = if state.enable_assume_hooks_follow_rules_of_react {
                    "BuiltInDefaultNonmutatingHookId"
                } else {
                    "BuiltInDefaultMutatingHookId"
                };
                signature = get_signature_for_shape_id(shape_id);
            }
            if debug_apply {
                eprintln!(
                    "[APPLY_SIG_METHOD] instr#{} receiver_id={} receiver_ty={:?} property_id={} property_ty={:?} method_name={:?} has_sig={}",
                    instr.id.0,
                    receiver.identifier.id.0,
                    receiver.identifier.type_,
                    property.identifier.id.0,
                    property.identifier.type_,
                    method_name,
                    signature.is_some()
                );
            }
            let apply_args = args
                .iter()
                .map(|a| match a {
                    Argument::Place(p) => ApplyArg::Place(p.clone()),
                    Argument::Spread(p) => ApplyArg::Spread(p.clone()),
                })
                .collect();
            effects.push(AliasingEffect::Apply {
                receiver: receiver.clone(),
                function: property.clone(),
                mutates_function: false,
                args: apply_args,
                into: Box::new(lvalue.clone()),
                signature,
                loc: value.loc().clone(),
            });
        }

        InstructionValue::NewExpression { callee, args, .. } => {
            let callee_global_name = resolve_global_name(ctx, callee);
            let signature = get_function_call_signature(&callee.identifier.type_).or_else(|| {
                callee_global_name
                    .as_deref()
                    .and_then(get_known_global_function_signature)
            });
            let apply_args = args
                .iter()
                .map(|a| match a {
                    Argument::Place(p) => ApplyArg::Place(p.clone()),
                    Argument::Spread(p) => ApplyArg::Spread(p.clone()),
                })
                .collect();
            effects.push(AliasingEffect::Apply {
                receiver: callee.clone(),
                function: callee.clone(),
                mutates_function: false,
                args: apply_args,
                into: Box::new(lvalue.clone()),
                signature,
                loc: value.loc().clone(),
            });
        }

        InstructionValue::PropertyDelete { object, .. }
        | InstructionValue::ComputedDelete { object, .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
            effects.push(AliasingEffect::Mutate {
                value: object.clone(),
                reason: None,
            });
        }

        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::ComputedLoad { object, .. } => {
            if is_primitive_type(&lvalue.identifier) {
                effects.push(AliasingEffect::Create {
                    into: lvalue.clone(),
                    value: ValueKind::Primitive,
                    reason: ValueReason::Other,
                });
            } else {
                effects.push(AliasingEffect::CreateFrom {
                    from: object.clone(),
                    into: lvalue.clone(),
                });
            }
        }

        InstructionValue::PropertyStore {
            object,
            value: val,
            property,
            ..
        } => {
            let mutation_reason = if matches!(property, PropertyLiteral::String(s) if s == "current")
            {
                if matches!(object.identifier.type_, Type::TypeVar { .. }) {
                    Some(MutationReason::AssignCurrentProperty)
                } else {
                    None
                }
            } else {
                None
            };
            effects.push(AliasingEffect::Mutate {
                value: object.clone(),
                reason: mutation_reason,
            });
            effects.push(AliasingEffect::Capture {
                from: val.clone(),
                into: object.clone(),
            });
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::ComputedStore {
            object, value: val, ..
        } => {
            effects.push(AliasingEffect::Mutate {
                value: object.clone(),
                reason: None,
            });
            effects.push(AliasingEffect::Capture {
                from: val.clone(),
                into: object.clone(),
            });
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::FunctionExpression {
            name, lowered_func, ..
        } => {
            if let Some(function_name) = name
                && lowered_function_has_shadowed_named_local(lowered_func, function_name)
            {
                if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                    eprintln!(
                        "[BAILOUT_REASON] infer-mutation shadowed-function-local name={} lvalue_id={} decl={}",
                        function_name, lvalue.identifier.id.0, lvalue.identifier.declaration_id.0
                    );
                }
                effects.push(AliasingEffect::Impure {
                    place: lvalue.clone(),
                    error: CompilerDiagnostic {
                        severity: DiagnosticSeverity::Invariant,
                        message:
                            "[InferMutationAliasingEffects] Expected value kind to be initialized"
                                .to_string(),
                    },
                });
                return effects;
            }

            let captures: Vec<Place> = lowered_func
                .func
                .context
                .iter()
                .filter(|p| p.effect == Effect::Capture || is_hoisted_setter_like_capture(ctx, p))
                .cloned()
                .collect();
            let signature = build_signature_from_function_expression(lowered_func);
            effects.push(AliasingEffect::CreateFunction {
                into: lvalue.clone(),
                captures,
                signature,
                context: lowered_func.func.context.clone(),
            });
        }

        InstructionValue::ObjectMethod { lowered_func, .. } => {
            let captures: Vec<Place> = lowered_func
                .func
                .context
                .iter()
                .filter(|p| p.effect == Effect::Capture || is_hoisted_setter_like_capture(ctx, p))
                .cloned()
                .collect();
            let signature = build_signature_from_function_expression(lowered_func);
            effects.push(AliasingEffect::CreateFunction {
                into: lvalue.clone(),
                captures,
                signature,
                context: lowered_func.func.context.clone(),
            });
        }

        InstructionValue::GetIterator { collection, .. } => {
            if std::env::var("DEBUG_GET_ITERATOR_TYPES").is_ok() {
                let resolved = resolve_decl_type_hint(ctx, collection.identifier.declaration_id)
                    .cloned()
                    .unwrap_or(Type::Poly);
                eprintln!(
                    "[GET_ITERATOR] instr#{} collection_id={} decl={} ident_ty={:?} resolved_ty={:?} builtin={}",
                    instr.id.0,
                    collection.identifier.id.0,
                    collection.identifier.declaration_id.0,
                    collection.identifier.type_,
                    resolved,
                    is_builtin_collection_type(ctx, &collection.identifier)
                );
            }
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            });
            if is_builtin_collection_type(ctx, &collection.identifier) {
                effects.push(AliasingEffect::Capture {
                    from: collection.clone(),
                    into: lvalue.clone(),
                });
            } else {
                effects.push(AliasingEffect::Alias {
                    from: collection.clone(),
                    into: lvalue.clone(),
                });
                effects.push(AliasingEffect::MutateTransitiveConditionally {
                    value: collection.clone(),
                });
            }
        }

        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            effects.push(AliasingEffect::MutateConditionally {
                value: iterator.clone(),
            });
            effects.push(AliasingEffect::CreateFrom {
                from: collection.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::NextPropertyOf { .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Frozen,
                reason: ValueReason::JsxCaptured,
            });
            // Freeze and capture all operands
            for_each_jsx_operand(tag, props, children, |operand| {
                effects.push(AliasingEffect::Freeze {
                    value: operand.clone(),
                    reason: ValueReason::JsxCaptured,
                });
                effects.push(AliasingEffect::Capture {
                    from: operand.clone(),
                    into: lvalue.clone(),
                });
            });
            // Render effects for tag and children
            if let JsxTag::Component(tag_place) = tag {
                effects.push(AliasingEffect::Render {
                    place: tag_place.clone(),
                });
            }
            if let Some(children) = children {
                for child in children {
                    effects.push(AliasingEffect::Render {
                        place: child.clone(),
                    });
                }
            }
            for prop in props {
                if let JsxAttribute::Attribute { place, .. } = prop {
                    let is_jsx_returning_function = match &place.identifier.type_ {
                        Type::Function { return_type, .. } => {
                            type_maybe_contains_jsx(return_type.as_ref())
                        }
                        _ => false,
                    }
                        || declaration_resolves_to_jsx_returning_function(
                            ctx,
                            place.identifier.declaration_id,
                        );
                    if is_jsx_returning_function {
                        // JSX-returning function props are render helpers and are
                        // assumed to execute during render.
                        effects.push(AliasingEffect::Render {
                            place: place.clone(),
                        });
                    }
                }
            }
        }

        InstructionValue::JsxFragment { children, .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Frozen,
                reason: ValueReason::JsxCaptured,
            });
            for child in children {
                effects.push(AliasingEffect::Freeze {
                    value: child.clone(),
                    reason: ValueReason::JsxCaptured,
                });
                effects.push(AliasingEffect::Capture {
                    from: child.clone(),
                    into: lvalue.clone(),
                });
            }
        }

        InstructionValue::DeclareLocal { lvalue: lval, .. } => {
            let decl_kind = if ctx
                .captured_context_declarations
                .contains(&lval.place.identifier.declaration_id)
            {
                let resolved = resolve_captured_context_place(ctx, &lval.place);
                if std::env::var("DEBUG_CONTEXT_CAPTURE").is_ok() {
                    eprintln!(
                        "[CONTEXT_CAPTURE] infer declarelocal as mutable decl={} id={} resolved_id={} name={}",
                        lval.place.identifier.declaration_id.0,
                        lval.place.identifier.id.0,
                        resolved.identifier.id.0,
                        lval.place
                            .identifier
                            .name
                            .as_ref()
                            .map_or("<none>".to_string(), |n| n.value().to_string())
                    );
                }
                ValueKind::Mutable
            } else {
                ValueKind::Primitive
            };
            let target = if decl_kind == ValueKind::Mutable {
                resolve_captured_context_place(ctx, &lval.place)
            } else {
                lval.place.clone()
            };
            effects.push(AliasingEffect::Create {
                into: target,
                value: decl_kind,
                reason: ValueReason::Other,
            });
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::DeclareContext { lvalue: lval, .. } => {
            let kind = lval.kind;
            if !ctx
                .hoisted_context_declarations
                .contains(&lval.place.identifier.declaration_id)
                || kind == InstructionKind::HoistedConst
                || kind == InstructionKind::HoistedFunction
                || kind == InstructionKind::HoistedLet
            {
                effects.push(AliasingEffect::Create {
                    into: lval.place.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::Other,
                });
            } else {
                effects.push(AliasingEffect::Mutate {
                    value: lval.place.clone(),
                    reason: None,
                });
            }
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::StoreContext {
            lvalue: lval,
            value: val,
            ..
        } => {
            if lval.kind == InstructionKind::Reassign
                || ctx
                    .hoisted_context_declarations
                    .contains(&lval.place.identifier.declaration_id)
            {
                effects.push(AliasingEffect::Mutate {
                    value: lval.place.clone(),
                    reason: None,
                });
            } else {
                effects.push(AliasingEffect::Create {
                    into: lval.place.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::Other,
                });
            }
            effects.push(AliasingEffect::Capture {
                from: val.clone(),
                into: lval.place.clone(),
            });
            effects.push(AliasingEffect::Assign {
                from: val.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::LoadContext { place, .. } => {
            effects.push(AliasingEffect::CreateFrom {
                from: place.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::LoadLocal { place, .. } => {
            if ctx.is_function_expression
                && ctx
                    .captured_context_declarations
                    .contains(&place.identifier.declaration_id)
            {
                let from = resolve_captured_context_place(ctx, place);
                effects.push(AliasingEffect::CreateFrom {
                    from,
                    into: lvalue.clone(),
                });
            } else {
                effects.push(AliasingEffect::Assign {
                    from: place.clone(),
                    into: lvalue.clone(),
                });
            }
        }

        InstructionValue::StoreLocal {
            lvalue: lval,
            value: val,
            ..
        } => {
            if is_reassign_to_outer_named_identifier(&lval.place, ctx, lval.kind) {
                effects.push(AliasingEffect::MutateGlobal {
                    place: lval.place.clone(),
                    error: global_reassignment_diagnostic(&variable_name_for_error(&lval.place)),
                });
            } else {
                if ctx.is_function_expression
                    && ctx
                        .captured_context_declarations
                        .contains(&lval.place.identifier.declaration_id)
                {
                    let target_place = resolve_captured_context_place(ctx, &lval.place);
                    if lval.kind == InstructionKind::Reassign
                        || ctx
                            .hoisted_context_declarations
                            .contains(&lval.place.identifier.declaration_id)
                    {
                        effects.push(AliasingEffect::Mutate {
                            value: target_place.clone(),
                            reason: None,
                        });
                    } else {
                        effects.push(AliasingEffect::Create {
                            into: target_place.clone(),
                            value: ValueKind::Mutable,
                            reason: ValueReason::Other,
                        });
                    }
                    effects.push(AliasingEffect::Capture {
                        from: val.clone(),
                        into: target_place,
                    });
                } else {
                    effects.push(AliasingEffect::Assign {
                        from: val.clone(),
                        into: lval.place.clone(),
                    });
                }
            }
            effects.push(AliasingEffect::Assign {
                from: val.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::Destructure {
            lvalue: lval,
            value: val,
            ..
        } => {
            // For each pattern item, create/assign from the value
            for_each_pattern_item(&lval.pattern, |item_place, is_spread| {
                if ctx
                    .hoisted_context_declarations
                    .contains(&item_place.identifier.declaration_id)
                {
                    if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                        eprintln!(
                            "[HOISTED_CTX_MUTATE] instr={} decl={} id={} kind={:?}",
                            instr.id.0,
                            item_place.identifier.declaration_id.0,
                            item_place.identifier.id.0,
                            lval.kind
                        );
                    }
                    effects.push(AliasingEffect::Mutate {
                        value: item_place.clone(),
                        reason: None,
                    });
                    effects.push(AliasingEffect::Capture {
                        from: val.clone(),
                        into: item_place.clone(),
                    });
                    return;
                }

                if ctx
                    .captured_context_declarations
                    .contains(&item_place.identifier.declaration_id)
                    && lval.kind != InstructionKind::Reassign
                    && ctx
                        .reassigned_declarations
                        .contains(&item_place.identifier.declaration_id)
                {
                    let target_place = resolve_captured_context_place(ctx, item_place);
                    effects.push(AliasingEffect::Create {
                        into: target_place.clone(),
                        value: ValueKind::Mutable,
                        reason: ValueReason::Other,
                    });
                    effects.push(AliasingEffect::Capture {
                        from: val.clone(),
                        into: target_place,
                    });
                    return;
                }

                if is_reassign_to_outer_named_identifier(item_place, ctx, lval.kind) {
                    effects.push(AliasingEffect::MutateGlobal {
                        place: item_place.clone(),
                        error: global_reassignment_diagnostic(&variable_name_for_error(item_place)),
                    });
                } else if is_primitive_type(&item_place.identifier) {
                    effects.push(AliasingEffect::Create {
                        into: item_place.clone(),
                        value: ValueKind::Primitive,
                        reason: ValueReason::Other,
                    });
                } else if is_spread {
                    effects.push(AliasingEffect::Create {
                        into: item_place.clone(),
                        value: ValueKind::Mutable,
                        reason: ValueReason::Other,
                    });
                    effects.push(AliasingEffect::Capture {
                        from: val.clone(),
                        into: item_place.clone(),
                    });
                } else {
                    effects.push(AliasingEffect::CreateFrom {
                        from: val.clone(),
                        into: item_place.clone(),
                    });
                }
            });
            effects.push(AliasingEffect::Assign {
                from: val.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::PostfixUpdate {
            lvalue: upd_lvalue, ..
        }
        | InstructionValue::PrefixUpdate {
            lvalue: upd_lvalue, ..
        } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
            effects.push(AliasingEffect::Create {
                into: upd_lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::StoreGlobal {
            name, value: val, ..
        } => {
            effects.push(AliasingEffect::MutateGlobal {
                place: val.clone(),
                error: global_reassignment_diagnostic(&format!("`{name}`")),
            });
            effects.push(AliasingEffect::Assign {
                from: val.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::TypeCastExpression { value: val, .. } => {
            effects.push(AliasingEffect::Assign {
                from: val.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::LoadGlobal { .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Global,
                reason: ValueReason::Global,
            });
        }

        InstructionValue::StartMemoize { deps, .. } => {
            if state.enable_preserve_existing_memoization_guarantees
                && let Some(deps) = deps
            {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(place) = &dep.root {
                        effects.push(AliasingEffect::Freeze {
                            value: place.clone(),
                            reason: ValueReason::HookCaptured,
                        });
                    }
                }
            }
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::FinishMemoize { decl, .. } => {
            if state.enable_preserve_existing_memoization_guarantees {
                effects.push(AliasingEffect::Freeze {
                    value: decl.clone(),
                    reason: ValueReason::HookCaptured,
                });
            }
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::TaggedTemplateExpression { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::UnaryExpression { .. } => {
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::Other,
            });
        }

        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            // Ternary: result aliases one of the branches. We model the
            // abstract value kind as the join of both branch values, matching
            // upstream phi-like behavior more closely than always-Mutable.
            let consequent_kind = state.kind(consequent.identifier.id);
            let alternate_kind = state.kind(alternate.identifier.id);
            let merged_kind = merge_value_kinds(consequent_kind.kind, alternate_kind.kind);
            let mut merged_reasons = consequent_kind.reasons.clone();
            merged_reasons.extend(alternate_kind.reasons.iter().copied());
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: merged_kind,
                reason: merged_reasons
                    .iter()
                    .copied()
                    .next()
                    .unwrap_or(ValueReason::Other),
            });
            effects.push(AliasingEffect::Capture {
                from: consequent.clone(),
                into: lvalue.clone(),
            });
            effects.push(AliasingEffect::Capture {
                from: alternate.clone(),
                into: lvalue.clone(),
            });
        }

        InstructionValue::LogicalExpression { left, right, .. } => {
            // Logical expressions likewise produce one of the two operand
            // values; use the abstract join rather than hard-coding Mutable.
            let left_kind = state.kind(left.identifier.id);
            let right_kind = state.kind(right.identifier.id);
            let merged_kind = merge_value_kinds(left_kind.kind, right_kind.kind);
            let mut merged_reasons = left_kind.reasons.clone();
            merged_reasons.extend(right_kind.reasons.iter().copied());
            effects.push(AliasingEffect::Create {
                into: lvalue.clone(),
                value: merged_kind,
                reason: merged_reasons
                    .iter()
                    .copied()
                    .next()
                    .unwrap_or(ValueReason::Other),
            });
            effects.push(AliasingEffect::Capture {
                from: left.clone(),
                into: lvalue.clone(),
            });
            effects.push(AliasingEffect::Capture {
                from: right.clone(),
                into: lvalue.clone(),
            });
        }
        InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. } => {}
    }

    effects
}

fn resolve_captured_context_place(ctx: &InferContext, place: &Place) -> Place {
    ctx.captured_context_place_by_declaration
        .get(&place.identifier.declaration_id)
        .cloned()
        .unwrap_or_else(|| place.clone())
}

// ---------------------------------------------------------------------------
// Compute effects for legacy signature
// ---------------------------------------------------------------------------

/// Creates a set of aliasing effects given a legacy FunctionSignature.
fn compute_effects_for_legacy_signature(
    ctx: &InferContext,
    state: &InferenceState,
    signature: &FunctionSignature,
    lvalue: &Place,
    receiver: &Place,
    args: &[ApplyArg],
    _loc: &SourceLocation,
) -> Vec<AliasingEffect> {
    let debug_apply = std::env::var("DEBUG_APPLY_SIGNATURE").is_ok();
    let return_value_reason = signature.return_value_reason.unwrap_or(ValueReason::Other);
    let mut effects = Vec::new();

    effects.push(AliasingEffect::Create {
        into: lvalue.clone(),
        value: signature.return_value_kind,
        reason: return_value_reason,
    });

    if signature.impure && state.validate_no_impure_functions_in_render {
        effects.push(AliasingEffect::Impure {
            place: receiver.clone(),
            error: CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: format!(
                    "Cannot call impure function{}during render",
                    signature
                        .canonical_name
                        .as_deref()
                        .map(|n| format!(" `{}` ", n))
                        .unwrap_or(" ".to_string())
                ),
            },
        });
    }

    if state.is_inferred_memo_enabled
        && let Some(reason) = signature.known_incompatible.as_ref()
    {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[BAILOUT_REASON] known-incompatible callee={} decl={} msg={}",
                receiver
                    .identifier
                    .name
                    .as_ref()
                    .map_or("<anonymous>", |n| n.value()),
                receiver.identifier.declaration_id.0,
                reason
            );
        }
        effects.push(AliasingEffect::Impure {
            place: receiver.clone(),
            error: CompilerDiagnostic {
                severity: DiagnosticSeverity::InvalidReact,
                message: reason.clone(),
            },
        });
    }

    // Upstream behavior: for mutableOnlyIfOperandsAreMutable signatures, if all
    // arguments are immutable/non-mutating then treat the call as pure capture
    // and return early.
    let args_immutable_non_mutating = if signature.mutable_only_if_operands_are_mutable {
        are_arguments_immutable_and_non_mutating(ctx, state, args)
    } else {
        false
    };
    if debug_apply {
        let receiver_kind = state.kind(receiver.identifier.id);
        let arg_kinds: Vec<String> = args
            .iter()
            .enumerate()
            .filter_map(|(idx, arg)| {
                let place = match arg {
                    ApplyArg::Hole => return None,
                    ApplyArg::Place(p) | ApplyArg::Spread(p) => p,
                };
                let kind = state.kind(place.identifier.id);
                Some(format!(
                    "#{} id={} decl={} kind={:?} reasons={:?}",
                    idx,
                    place.identifier.id.0,
                    place.identifier.declaration_id.0,
                    kind.kind,
                    kind.reasons
                ))
            })
            .collect();
        eprintln!(
            "[APPLY_SIG_LEGACY] signature={:?} receiver_id={} receiver_kind={:?} receiver_reasons={:?} args=[{}] mutable_only_if_operands_are_mutable={} args_immutable_non_mutating={}",
            signature.canonical_name,
            receiver.identifier.id.0,
            receiver_kind.kind,
            receiver_kind.reasons,
            arg_kinds.join(", "),
            signature.mutable_only_if_operands_are_mutable,
            args_immutable_non_mutating
        );
    }

    if signature.mutable_only_if_operands_are_mutable && args_immutable_non_mutating {
        effects.push(AliasingEffect::Alias {
            from: receiver.clone(),
            into: lvalue.clone(),
        });
        for arg in args {
            let place = match arg {
                ApplyArg::Hole => continue,
                ApplyArg::Place(place) | ApplyArg::Spread(place) => place,
            };
            effects.push(AliasingEffect::ImmutableCapture {
                from: place.clone(),
                into: lvalue.clone(),
            });
        }
        return effects;
    }

    // Unless callee effect is Capture, alias receiver into lvalue.
    // Some built-in method signatures (e.g. `concat`) are marked no_alias
    // to model a fresh return value that should not alias the receiver.
    if !signature.no_alias && signature.callee_effect != Effect::Capture {
        effects.push(AliasingEffect::Alias {
            from: receiver.clone(),
            into: lvalue.clone(),
        });
    }

    let mut stores: Vec<Place> = Vec::new();
    let mut captures: Vec<Place> = Vec::new();

    // Visit callee
    visit_signature_effect(
        &mut effects,
        receiver,
        signature.callee_effect,
        lvalue,
        &mut stores,
        &mut captures,
        return_value_reason,
    );

    // Visit args
    let args_places: Vec<(&Place, Effect)> = args
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| match arg {
            ApplyArg::Hole => None,
            ApplyArg::Place(p) => {
                let sig_effect = if i < signature.positional_params.len() {
                    signature.positional_params[i]
                } else {
                    signature.rest_param.unwrap_or(Effect::ConditionallyMutate)
                };
                Some((p, sig_effect))
            }
            ApplyArg::Spread(p) => {
                let signature_effect = if i < signature.positional_params.len() {
                    signature.positional_params[i]
                } else {
                    signature.rest_param.unwrap_or(Effect::ConditionallyMutate)
                };
                let sig_effect = get_argument_effect_for_spread(signature_effect);
                Some((p, sig_effect))
            }
        })
        .collect();

    for (place, eff) in args_places {
        visit_signature_effect(
            &mut effects,
            place,
            eff,
            lvalue,
            &mut stores,
            &mut captures,
            return_value_reason,
        );
    }

    // Handle captures
    if !captures.is_empty() {
        if stores.is_empty() {
            for cap in &captures {
                effects.push(AliasingEffect::Alias {
                    from: cap.clone(),
                    into: lvalue.clone(),
                });
            }
        } else {
            for cap in &captures {
                for store in &stores {
                    effects.push(AliasingEffect::Capture {
                        from: cap.clone(),
                        into: store.clone(),
                    });
                }
            }
        }
    }

    effects
}

fn visit_signature_effect(
    effects: &mut Vec<AliasingEffect>,
    place: &Place,
    effect: Effect,
    lvalue: &Place,
    stores: &mut Vec<Place>,
    captures: &mut Vec<Place>,
    return_value_reason: ValueReason,
) {
    match effect {
        Effect::Store => {
            effects.push(AliasingEffect::Mutate {
                value: place.clone(),
                reason: None,
            });
            stores.push(place.clone());
        }
        Effect::Capture => {
            captures.push(place.clone());
        }
        Effect::ConditionallyMutate => {
            effects.push(AliasingEffect::MutateTransitiveConditionally {
                value: place.clone(),
            });
        }
        Effect::ConditionallyMutateIterator => {
            if let Some(mutate_iterator) = conditionally_mutate_iterator(place) {
                effects.push(mutate_iterator);
            }
            effects.push(AliasingEffect::Capture {
                from: place.clone(),
                into: lvalue.clone(),
            });
        }
        Effect::Freeze => {
            effects.push(AliasingEffect::Freeze {
                value: place.clone(),
                reason: return_value_reason,
            });
        }
        Effect::Mutate => {
            effects.push(AliasingEffect::MutateTransitive {
                value: place.clone(),
            });
        }
        Effect::Read => {
            effects.push(AliasingEffect::ImmutableCapture {
                from: place.clone(),
                into: lvalue.clone(),
            });
        }
        Effect::Unknown => {
            // Treat unknown as conditionally mutate
            effects.push(AliasingEffect::MutateTransitiveConditionally {
                value: place.clone(),
            });
        }
    }
}

fn get_argument_effect_for_spread(sig_effect: Effect) -> Effect {
    match sig_effect {
        Effect::Mutate | Effect::ConditionallyMutate => sig_effect,
        _ => Effect::ConditionallyMutateIterator,
    }
}

fn is_known_mutable_effect(effect: Effect) -> bool {
    match effect {
        Effect::Store | Effect::ConditionallyMutate | Effect::ConditionallyMutateIterator => true,
        Effect::Mutate => true,
        Effect::Read | Effect::Capture | Effect::Freeze => false,
        Effect::Unknown => {
            unreachable!("Unexpected unknown effect in known mutable effect check")
        }
    }
}

fn aliasing_signature_mutates_inputs(signature: &AliasingSignature) -> bool {
    let mut input_ids: HashSet<IdentifierId> = signature.params.iter().copied().collect();
    if let Some(rest) = signature.rest {
        input_ids.insert(rest);
    }
    signature.effects.iter().any(|effect| match effect {
        AliasingEffect::Mutate { value, .. }
        | AliasingEffect::MutateConditionally { value }
        | AliasingEffect::MutateTransitive { value }
        | AliasingEffect::MutateTransitiveConditionally { value } => {
            input_ids.contains(&value.identifier.id)
        }
        _ => false,
    })
}

fn are_arguments_immutable_and_non_mutating(
    ctx: &InferContext,
    state: &InferenceState,
    args: &[ApplyArg],
) -> bool {
    for arg in args {
        let place = match arg {
            ApplyArg::Hole => continue,
            ApplyArg::Place(p) | ApplyArg::Spread(p) => p,
        };
        if let Some(global_name) = ctx
            .global_name_by_declaration
            .get(&place.identifier.declaration_id)
            && let Some(fn_shape) =
                get_known_global_call_signature(Some(global_name.as_str()), &place.identifier)
        {
            // Match upstream's early return for function-typed args with known signatures.
            return !fn_shape
                .positional_params
                .iter()
                .copied()
                .any(is_known_mutable_effect)
                && fn_shape
                    .rest_param
                    .is_none_or(|effect| !is_known_mutable_effect(effect));
        }
        if matches!(place.identifier.type_, Type::Function { .. })
            && let Some(fn_shape) = get_function_call_signature(&place.identifier.type_)
        {
            // Upstream behavior: for function-typed args with known signatures,
            // return early based on whether any parameter is known mutable.
            return !fn_shape
                .positional_params
                .iter()
                .copied()
                .any(is_known_mutable_effect)
                && fn_shape
                    .rest_param
                    .is_none_or(|effect| !is_known_mutable_effect(effect));
        }
        let kind = state.kind(place.identifier.id);
        match kind.kind {
            ValueKind::Primitive | ValueKind::Frozen => {}
            _ => return false,
        }
        if let Some(local_sig) = state.local_function_signature(place.identifier.id)
            && aliasing_signature_mutates_inputs(&local_sig.signature)
        {
            // Frozen local lambdas still count as mutating args if they may mutate
            // their own inputs.
            return false;
        }
    }
    true
}

fn value_kind_for_identifier(ident: &Identifier) -> ValueKind {
    match ident.type_ {
        Type::Primitive => ValueKind::Primitive,
        _ => ValueKind::Mutable,
    }
}

fn get_known_method_signature(
    ctx: &InferContext,
    receiver: &Place,
    method_name: Option<&str>,
    lvalue_ident: &Identifier,
) -> Option<FunctionSignature> {
    use crate::hir::globals::GlobalRegistry;
    use crate::hir::object_shape::PropertyType;

    // Try shape-based method lookup first (mirrors upstream env.getFunctionSignature behavior).
    if let Some(method_name) = method_name {
        let receiver_shape_id = match &receiver.identifier.type_ {
            Type::Object {
                shape_id: Some(shape_id),
            } => Some(shape_id.as_str()),
            Type::Function {
                shape_id: Some(shape_id),
                ..
            } => Some(shape_id.as_str()),
            _ => None,
        }
        .or_else(|| {
            // Inner lowered functions often load captured values through fresh
            // temporaries that still carry TypeVar. Follow declaration-based
            // type hints (including LoadContext chains) before giving up.
            resolve_decl_type_hint(ctx, receiver.identifier.declaration_id).and_then(
                |ty| match ty {
                    Type::Object {
                        shape_id: Some(shape_id),
                    } => Some(shape_id.as_str()),
                    Type::Function {
                        shape_id: Some(shape_id),
                        ..
                    } => Some(shape_id.as_str()),
                    _ => None,
                },
            )
        });
        if let Some(shape_id) = receiver_shape_id {
            let globals = GlobalRegistry::new();
            if let Some(property_type) = globals.shapes.get_property(shape_id, method_name) {
                if let PropertyType::Function(signature) = property_type {
                    return Some(signature.clone());
                }
            }
        }
    }

    // `concat` returns a fresh container value and should not alias the receiver.
    if method_name == Some("concat") {
        return Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_value_kind: value_kind_for_identifier(lvalue_ident),
            callee_effect: Effect::Read,
            no_alias: true,
            ..Default::default()
        });
    }

    None
}

fn resolve_method_name(ctx: &InferContext, property: &Place) -> Option<String> {
    match property.identifier.name.as_ref() {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            Some(name.clone())
        }
        None => ctx
            .method_name_by_declaration
            .get(&property.identifier.declaration_id)
            .cloned(),
    }
}

fn resolve_global_name(ctx: &InferContext, callee: &Place) -> Option<String> {
    if let Some(mapped) = ctx
        .global_name_by_declaration
        .get(&callee.identifier.declaration_id)
    {
        return Some(mapped.clone());
    }

    match callee.identifier.name.as_ref() {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            Some(name.clone())
        }
        None => None,
    }
}

fn get_known_global_call_signature(
    global_name: Option<&str>,
    _lvalue_ident: &Identifier,
) -> Option<FunctionSignature> {
    match global_name {
        Some("ReactCompilerKnownIncompatibleTest::useKnownIncompatible") => {
            Some(FunctionSignature {
                rest_param: Some(Effect::Read),
                return_type: ReturnType::Poly,
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                hook_kind: Some(HookKind::Custom),
                known_incompatible: Some(
                    "useKnownIncompatible is known to be incompatible".to_string(),
                ),
                ..Default::default()
            })
        }
        Some("ReactCompilerKnownIncompatibleTest::knownIncompatible") => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            known_incompatible: Some(
                "useKnownIncompatible is known to be incompatible".to_string(),
            ),
            ..Default::default()
        }),
        Some("ReactCompilerKnownIncompatibleTest::useKnownIncompatibleIndirect") => {
            Some(FunctionSignature {
                rest_param: Some(Effect::Read),
                return_type: ReturnType::Object {
                    shape_id: TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
                },
                return_value_kind: ValueKind::Frozen,
                callee_effect: Effect::Read,
                hook_kind: Some(HookKind::Custom),
                ..Default::default()
            })
        }
        Some("shared-runtime::typedArrayPush" | "typedArrayPush") => Some(FunctionSignature {
            positional_params: vec![Effect::Store, Effect::Capture],
            rest_param: Some(Effect::Capture),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        }),
        Some("shared-runtime::typedLog" | "typedLog" | "shared-runtime::default") => {
            Some(FunctionSignature {
                positional_params: vec![],
                rest_param: Some(Effect::Read),
                return_type: ReturnType::Primitive,
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            })
        }
        Some(
            "Boolean" | "Number" | "String" | "parseInt" | "parseFloat" | "isNaN" | "isFinite"
            | "encodeURI" | "encodeURIComponent" | "decodeURI" | "decodeURIComponent",
        ) => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        }),
        _ => None,
    }
}

fn get_known_global_function_signature(global_name: &str) -> Option<FunctionSignature> {
    use crate::hir::globals::{GlobalKind, GlobalRegistry};

    let globals = GlobalRegistry::new();
    let resolve = |name: &str| -> Option<FunctionSignature> {
        let global = globals.get_global(name)?;
        match &global.kind {
            GlobalKind::Function(sig) | GlobalKind::Hook(sig) => Some(sig.clone()),
            _ => None,
        }
    };

    resolve(global_name).or_else(|| {
        // Names can be lowered as `module::name` (e.g. `react::useEffect`).
        // Registry keys use canonical global names (`useEffect`), so try suffix.
        global_name
            .rsplit_once("::")
            .and_then(|(_, suffix)| resolve(suffix))
    })
}

fn nth_apply_arg_place(args: &[ApplyArg], idx: usize) -> Option<Place> {
    match args.get(idx)? {
        ApplyArg::Place(place) | ApplyArg::Spread(place) => Some(place.clone()),
        ApplyArg::Hole => None,
    }
}

fn compute_known_global_aliasing_effects(
    global_name: &str,
    into: &Place,
    args: &[ApplyArg],
) -> Option<Vec<AliasingEffect>> {
    match global_name {
        "react::useEffect" | "useEffect" => {
            // Upstream BuiltInUseEffectHook aliasing:
            // - Freeze all rest arguments (callback + deps) with Effect reason
            // - Return a primitive known-signature value
            let mut effects = Vec::with_capacity(args.len() + 1);
            for arg in args {
                let place = match arg {
                    ApplyArg::Place(place) | ApplyArg::Spread(place) => place,
                    ApplyArg::Hole => continue,
                };
                effects.push(AliasingEffect::Freeze {
                    value: place.clone(),
                    reason: ValueReason::Effect,
                });
            }
            effects.push(AliasingEffect::Create {
                into: into.clone(),
                value: ValueKind::Primitive,
                reason: ValueReason::KnownReturnSignature,
            });
            Some(effects)
        }
        "shared-runtime::typedIdentity" | "typedIdentity" => {
            let value = nth_apply_arg_place(args, 0)?;
            Some(vec![AliasingEffect::Assign {
                from: value,
                into: into.clone(),
            }])
        }
        "shared-runtime::typedAssign" | "typedAssign" => {
            let value = nth_apply_arg_place(args, 0)?;
            Some(vec![AliasingEffect::Assign {
                from: value,
                into: into.clone(),
            }])
        }
        "shared-runtime::typedAlias" | "typedAlias" => {
            let value = nth_apply_arg_place(args, 0)?;
            Some(vec![
                AliasingEffect::Create {
                    into: into.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::KnownReturnSignature,
                },
                AliasingEffect::Alias {
                    from: value,
                    into: into.clone(),
                },
            ])
        }
        "shared-runtime::typedCapture" | "typedCapture" => {
            let value = nth_apply_arg_place(args, 0)?;
            Some(vec![
                AliasingEffect::Create {
                    into: into.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::KnownReturnSignature,
                },
                AliasingEffect::Capture {
                    from: value,
                    into: into.clone(),
                },
            ])
        }
        "shared-runtime::typedCreateFrom" | "typedCreateFrom" => {
            let value = nth_apply_arg_place(args, 0)?;
            Some(vec![AliasingEffect::CreateFrom {
                from: value,
                into: into.clone(),
            }])
        }
        "shared-runtime::typedMutate" | "typedMutate" => {
            let object = nth_apply_arg_place(args, 0)?;
            let maybe_value = nth_apply_arg_place(args, 1);
            let mut effects = vec![
                AliasingEffect::Create {
                    into: into.clone(),
                    value: ValueKind::Primitive,
                    reason: ValueReason::KnownReturnSignature,
                },
                AliasingEffect::Mutate {
                    value: object.clone(),
                    reason: None,
                },
            ];
            if let Some(value) = maybe_value {
                effects.push(AliasingEffect::Capture {
                    from: value,
                    into: object,
                });
            }
            Some(effects)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Type helpers
// ---------------------------------------------------------------------------

fn is_primitive_type(ident: &Identifier) -> bool {
    matches!(ident.type_, Type::Primitive)
}

fn is_array_type(ident: &Identifier) -> bool {
    match &ident.type_ {
        Type::Object { shape_id } => shape_id.as_deref() == Some("BuiltInArray"),
        _ => false,
    }
}

fn is_map_type(ident: &Identifier) -> bool {
    match &ident.type_ {
        Type::Object { shape_id } => shape_id.as_deref() == Some("BuiltInMap"),
        _ => false,
    }
}

fn is_set_type(ident: &Identifier) -> bool {
    match &ident.type_ {
        Type::Object { shape_id } => shape_id.as_deref() == Some("BuiltInSet"),
        _ => false,
    }
}

fn is_jsx_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Object { shape_id } if shape_id.as_deref() == Some("BuiltInJsx")
    )
}

fn type_maybe_contains_jsx(ty: &Type) -> bool {
    if is_jsx_type(ty) {
        return true;
    }
    match ty {
        Type::Phi { operands } => operands.iter().any(type_maybe_contains_jsx),
        _ => false,
    }
}

fn declaration_resolves_to_jsx_returning_function(ctx: &InferContext, decl: DeclarationId) -> bool {
    let mut current = decl;
    let mut seen: HashSet<DeclarationId> = HashSet::new();
    loop {
        if !seen.insert(current) {
            return false;
        }
        if ctx.jsx_returning_function_declarations.contains(&current) {
            return true;
        }
        match ctx.load_source_by_declaration.get(&current).copied() {
            Some(next) => current = next,
            None => return false,
        }
    }
}

fn is_shape_type(ty: &Type, shape: &str) -> bool {
    matches!(ty, Type::Object { shape_id } if shape_id.as_deref() == Some(shape))
}

fn resolve_decl_type_hint(ctx: &InferContext, decl: DeclarationId) -> Option<&Type> {
    let mut current = decl;
    let mut seen: HashSet<DeclarationId> = HashSet::new();
    loop {
        if !seen.insert(current) {
            return None;
        }
        if let Some(ty) = ctx.declaration_type_by_declaration.get(&current)
            && !matches!(ty, Type::Poly | Type::TypeVar { .. })
        {
            return Some(ty);
        }
        match ctx.load_source_by_declaration.get(&current).copied() {
            Some(next) => current = next,
            None => return None,
        }
    }
}

fn is_builtin_collection_type(ctx: &InferContext, ident: &Identifier) -> bool {
    if is_array_type(ident) || is_map_type(ident) || is_set_type(ident) {
        return true;
    }
    if let Some(ty) = resolve_decl_type_hint(ctx, ident.declaration_id) {
        return is_shape_type(ty, "BuiltInArray")
            || is_shape_type(ty, "BuiltInMap")
            || is_shape_type(ty, "BuiltInSet");
    }
    false
}

/// Get the function call signature from a Type, if available.
pub(crate) fn get_function_call_signature(ty: &Type) -> Option<FunctionSignature> {
    match ty {
        Type::Function {
            shape_id: Some(sid),
            ..
        } => get_signature_for_shape_id(sid).or_else(|| get_method_property_signature(sid)),
        _ => None,
    }
}

const METHOD_SIGNATURE_SHAPE_PREFIX: &str = "MethodSignature|";

pub(crate) fn encode_method_signature_shape_id(receiver_shape_id: &str, property: &str) -> String {
    format!("{METHOD_SIGNATURE_SHAPE_PREFIX}{receiver_shape_id}|{property}")
}

fn get_method_property_signature(shape_id: &str) -> Option<FunctionSignature> {
    use crate::hir::globals::GlobalRegistry;
    use crate::hir::object_shape::PropertyType;

    let encoded = shape_id.strip_prefix(METHOD_SIGNATURE_SHAPE_PREFIX)?;
    let (receiver_shape_id, property) = encoded.split_once('|')?;
    if receiver_shape_id.is_empty() || property.is_empty() {
        return None;
    }

    let globals = GlobalRegistry::new();
    match globals.shapes.get_property(receiver_shape_id, property) {
        Some(PropertyType::Function(signature)) => Some(signature.clone()),
        _ => None,
    }
}

/// Resolve a shape_id to a known FunctionSignature.
fn get_signature_for_shape_id(shape_id: &str) -> Option<FunctionSignature> {
    use crate::hir::object_shape::*;
    use crate::hir::types::*;

    match shape_id {
        "BuiltInUseStateHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            return_value_reason: Some(ValueReason::State),
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseState),
            ..Default::default()
        }),
        "BuiltInUseReducerHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            return_value_reason: Some(ValueReason::ReducerState),
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseReducer),
            ..Default::default()
        }),
        "BuiltInUseContextHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_value_kind: ValueKind::Frozen,
            return_value_reason: Some(ValueReason::Context),
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseContext),
            ..Default::default()
        }),
        "BuiltInUseRefHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Capture),
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseRef),
            ..Default::default()
        }),
        // useState/useReducer dispatchers and useActionState setter:
        // freeze args, read callee, return primitive.
        "BuiltInSetState" | "BuiltInDispatch" | "BuiltInSetActionState" => {
            Some(FunctionSignature {
                rest_param: Some(Effect::Freeze),
                return_value_kind: ValueKind::Primitive,
                callee_effect: Effect::Read,
                ..Default::default()
            })
        }
        "BuiltInUseMemoHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseMemo),
            ..Default::default()
        }),
        "BuiltInUseCallbackHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseCallback),
            ..Default::default()
        }),
        "BuiltInUseEffectHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseEffect),
            ..Default::default()
        }),
        "BuiltInUseTransitionHookId" => Some(FunctionSignature {
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseTransition),
            ..Default::default()
        }),
        "BuiltInUseImperativeHandleHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseImperativeHandle),
            ..Default::default()
        }),
        "BuiltInUseActionStateHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            return_value_reason: Some(ValueReason::State),
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::UseActionState),
            ..Default::default()
        }),
        // Default mutating hook (custom hooks matching use* pattern when
        // `enableAssumeHooksFollowRulesOfReact` is disabled)
        "BuiltInDefaultMutatingHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::ConditionallyMutate),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            ..Default::default()
        }),
        // Default non-mutating hook (custom hooks matching use* pattern)
        "BuiltInDefaultNonmutatingHookId" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            ..Default::default()
        }),
        "ReactCompilerKnownIncompatibleHook" => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            known_incompatible: Some("useKnownIncompatible is known to be incompatible".to_string()),
            ..Default::default()
        }),
        "ReactCompilerKnownIncompatibleFunction" => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            known_incompatible: Some("useKnownIncompatible is known to be incompatible".to_string()),
            ..Default::default()
        }),
        "ReactCompilerKnownIncompatibleIndirectHook" => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Object {
                shape_id: TEST_KNOWN_INCOMPATIBLE_INDIRECT_RESULT_ID,
            },
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            ..Default::default()
        }),
        "ReactCompilerKnownIncompatibleIndirectFunction" => Some(FunctionSignature {
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            known_incompatible: Some(
                "useKnownIncompatibleIndirect returns an incompatible() function that is known incompatible"
                    .to_string(),
            ),
            ..Default::default()
        }),
        "ReactCompilerTestNotAHookTypedAsHook" => Some(FunctionSignature {
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            ..Default::default()
        }),
        TEST_SHARED_RUNTIME_GRAPHQL_FN_ID => Some(FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Read),
            return_type: ReturnType::Primitive,
            return_value_kind: ValueKind::Primitive,
            callee_effect: Effect::Read,
            ..Default::default()
        }),
        TEST_SHARED_RUNTIME_USE_FREEZE_HOOK_ID => Some(FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            ..Default::default()
        }),
        TEST_SHARED_RUNTIME_USE_FRAGMENT_HOOK_ID => Some(FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Object {
                shape_id: BUILT_IN_MIXED_READONLY_ID,
            },
            return_value_kind: ValueKind::Frozen,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            no_alias: true,
            ..Default::default()
        }),
        TEST_SHARED_RUNTIME_USE_NO_ALIAS_HOOK_ID => Some(FunctionSignature {
            positional_params: vec![],
            rest_param: Some(Effect::Freeze),
            return_type: ReturnType::Poly,
            return_value_kind: ValueKind::Mutable,
            callee_effect: Effect::Read,
            hook_kind: Some(HookKind::Custom),
            no_alias: true,
            ..Default::default()
        }),
        _ => {
            // Fall back to registered shape function types so env-gated module
            // extensions (e.g. reanimated custom type definitions) participate
            // in aliasing effect inference.
            use crate::hir::globals::GlobalRegistry;
            let globals = GlobalRegistry::new();
            globals
                .get_shape(shape_id)
                .and_then(|shape| shape.function_type.clone())
        }
    }
}

// ---------------------------------------------------------------------------
// JSX operand iteration helper
// ---------------------------------------------------------------------------

fn for_each_jsx_operand(
    tag: &JsxTag,
    props: &[JsxAttribute],
    children: &Option<Vec<Place>>,
    mut f: impl FnMut(&Place),
) {
    if let JsxTag::Component(p) = tag {
        f(p);
    }
    for attr in props {
        match attr {
            JsxAttribute::Attribute { place, .. } => f(place),
            JsxAttribute::SpreadAttribute { argument } => f(argument),
        }
    }
    if let Some(children) = children {
        for child in children {
            f(child);
        }
    }
}

// ---------------------------------------------------------------------------
// Pattern item iteration helper
// ---------------------------------------------------------------------------

fn for_each_pattern_item(pattern: &Pattern, mut f: impl FnMut(&Place, bool)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) => f(p, false),
                    ArrayElement::Spread(p) => f(p, true),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => f(&p.place, false),
                    ObjectPropertyOrSpread::Spread(p) => f(p, true),
                }
            }
        }
    }
}

fn is_reassign_to_outer_named_identifier(
    place: &Place,
    ctx: &InferContext,
    kind: InstructionKind,
) -> bool {
    kind == InstructionKind::Reassign
        && matches!(
            place.identifier.name,
            Some(IdentifierName::Named(_)) | Some(IdentifierName::Promoted(_))
        )
        && !ctx
            .local_declarations
            .contains(&place.identifier.declaration_id)
}

fn global_reassignment_diagnostic(variable: &str) -> CompilerDiagnostic {
    CompilerDiagnostic {
        severity: DiagnosticSeverity::InvalidReact,
        message: format!(
            "Cannot reassign variables declared outside of the component/hook ({variable} cannot be reassigned)"
        ),
    }
}

fn uninitialized_value_kind_diagnostic(place: &Place) -> CompilerDiagnostic {
    let variable = variable_name_for_error(place);
    CompilerDiagnostic {
        severity: DiagnosticSeverity::Invariant,
        message: format!(
            "[InferMutationAliasingEffects] Expected value kind to be initialized ({variable} is uninitialized)"
        ),
    }
}

fn should_allow_maybe_frozen_jsx_mutation(place: &Place, abs_val: &AbstractValue) -> bool {
    place.identifier.name.is_none()
        && abs_val.kind == ValueKind::MaybeFrozen
        && abs_val.reasons.len() == 2
        && abs_val.reasons.contains(&ValueReason::JsxCaptured)
        && abs_val.reasons.contains(&ValueReason::Other)
}

fn variable_name_for_error(place: &Place) -> String {
    match &place.identifier.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            format!("`{name}`")
        }
        None => "variable".to_string(),
    }
}

fn mutate_frozen_diagnostic(
    ctx: &InferContext,
    state: &InferenceState,
    value: &Place,
    is_assign_current_property: bool,
) -> CompilerDiagnostic {
    let variable = variable_name_for_error(value);
    if ctx
        .hoisted_context_declarations
        .contains(&value.identifier.declaration_id)
    {
        return CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: format!(
                "Cannot access variable before it is declared ({variable} is accessed before declaration)"
            ),
        };
    }

    let abs_val = state.kind(value.identifier.id);
    let mut message = format!(
        "{variable} cannot be modified: {}",
        write_error_reason_for_kind(abs_val.kind, &abs_val.reasons)
    );
    if is_assign_current_property {
        message.push_str(
            " Hint: if this value is a Ref (returned by useRef), rename it to end with `Ref`.",
        );
    }

    CompilerDiagnostic {
        severity: DiagnosticSeverity::InvalidReact,
        message,
    }
}

// ---------------------------------------------------------------------------
// Error messages
// ---------------------------------------------------------------------------

fn write_error_reason_for_kind(kind: ValueKind, reasons: &HashSet<ValueReason>) -> String {
    if reasons.contains(&ValueReason::Global) {
        "global".to_string()
    } else if reasons.contains(&ValueReason::JsxCaptured) {
        "value used in JSX".to_string()
    } else if reasons.contains(&ValueReason::Context) {
        "value returned from useContext()".to_string()
    } else if reasons.contains(&ValueReason::ReactiveFunctionArgument) {
        "component prop or hook argument".to_string()
    } else if reasons.contains(&ValueReason::State) {
        "value returned from useState()".to_string()
    } else if reasons.contains(&ValueReason::ReducerState) {
        "value returned from useReducer()".to_string()
    } else if reasons.contains(&ValueReason::HookCaptured) {
        "value captured by a hook".to_string()
    } else if reasons.contains(&ValueReason::HookReturn) {
        "value returned from a hook".to_string()
    } else if reasons.contains(&ValueReason::Effect) {
        "value used in an effect".to_string()
    } else {
        match kind {
            ValueKind::Frozen => "frozen".to_string(),
            ValueKind::Global => "global".to_string(),
            _ => "immutable".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_value_kinds_same() {
        assert_eq!(
            merge_value_kinds(ValueKind::Mutable, ValueKind::Mutable),
            ValueKind::Mutable
        );
        assert_eq!(
            merge_value_kinds(ValueKind::Frozen, ValueKind::Frozen),
            ValueKind::Frozen
        );
        assert_eq!(
            merge_value_kinds(ValueKind::Primitive, ValueKind::Primitive),
            ValueKind::Primitive
        );
    }

    #[test]
    fn test_merge_value_kinds_lattice() {
        // frozen | mutable => maybe-frozen
        assert_eq!(
            merge_value_kinds(ValueKind::Frozen, ValueKind::Mutable),
            ValueKind::MaybeFrozen
        );
        assert_eq!(
            merge_value_kinds(ValueKind::Mutable, ValueKind::Frozen),
            ValueKind::MaybeFrozen
        );

        // immutable (Primitive) | mutable => mutable
        assert_eq!(
            merge_value_kinds(ValueKind::Primitive, ValueKind::Mutable),
            ValueKind::Mutable
        );

        // immutable (Primitive) | frozen => frozen
        assert_eq!(
            merge_value_kinds(ValueKind::Primitive, ValueKind::Frozen),
            ValueKind::Frozen
        );

        // any | maybe-frozen => maybe-frozen
        assert_eq!(
            merge_value_kinds(ValueKind::Mutable, ValueKind::MaybeFrozen),
            ValueKind::MaybeFrozen
        );
        assert_eq!(
            merge_value_kinds(ValueKind::Primitive, ValueKind::MaybeFrozen),
            ValueKind::MaybeFrozen
        );

        // context | mutable => context
        assert_eq!(
            merge_value_kinds(ValueKind::Context, ValueKind::Mutable),
            ValueKind::Context
        );

        // context | frozen => maybe-frozen
        assert_eq!(
            merge_value_kinds(ValueKind::Context, ValueKind::Frozen),
            ValueKind::MaybeFrozen
        );

        // context | immutable => context
        assert_eq!(
            merge_value_kinds(ValueKind::Context, ValueKind::Primitive),
            ValueKind::Context
        );
    }

    #[test]
    fn test_inference_state_basic() {
        let mut state = InferenceState::empty(false, true, true, true);

        let id1 = IdentifierId::new(1);
        let vid = state.initialize(AbstractValue::new(ValueKind::Mutable, ValueReason::Other));
        state.define(id1, vid);

        assert!(state.is_defined(id1));
        let kind = state.kind(id1);
        assert_eq!(kind.kind, ValueKind::Mutable);
    }

    #[test]
    fn test_inference_state_freeze() {
        let mut state = InferenceState::empty(false, true, true, true);

        let id1 = IdentifierId::new(1);
        let vid = state.initialize(AbstractValue::new(ValueKind::Mutable, ValueReason::Other));
        state.define(id1, vid);

        let did_freeze = state.freeze(id1, ValueReason::JsxCaptured);
        assert!(did_freeze);

        let kind = state.kind(id1);
        assert_eq!(kind.kind, ValueKind::Frozen);

        // Freezing again should return false
        let did_freeze2 = state.freeze(id1, ValueReason::JsxCaptured);
        assert!(!did_freeze2);
    }

    #[test]
    fn test_inference_state_assign() {
        let mut state = InferenceState::empty(false, true, true, true);

        let id1 = IdentifierId::new(1);
        let id2 = IdentifierId::new(2);

        let vid = state.initialize(AbstractValue::new(ValueKind::Mutable, ValueReason::Other));
        state.define(id1, vid);
        state.assign(id2, id1);

        // Both should point to same value
        let kind1 = state.kind(id1);
        let kind2 = state.kind(id2);
        assert_eq!(kind1.kind, kind2.kind);

        // Freezing one should freeze the other (they share the same ValueId)
        state.freeze(id1, ValueReason::JsxCaptured);
        let kind2_after = state.kind(id2);
        assert_eq!(kind2_after.kind, ValueKind::Frozen);
    }

    #[test]
    fn test_inference_state_merge() {
        let mut state1 = InferenceState::empty(false, true, true, true);
        let mut state2 = InferenceState::empty(false, true, true, true);

        let id1 = IdentifierId::new(1);
        let vid1 = state1.initialize(AbstractValue::new(ValueKind::Mutable, ValueReason::Other));
        state1.define(id1, vid1);

        // state2 has a different value for the same identifier
        let id2 = IdentifierId::new(2);
        let vid2 = state2.initialize(AbstractValue::new(
            ValueKind::Frozen,
            ValueReason::JsxCaptured,
        ));
        state2.define(id2, vid2);

        // Merging should produce a new state with both
        let merged = state1.merge(&state2);
        assert!(merged.is_some());
        let merged = merged.unwrap();
        assert!(merged.is_defined(id1));
        assert!(merged.is_defined(id2));
    }

    #[test]
    fn test_mutation_result() {
        let mut state = InferenceState::empty(false, true, true, true);
        let id1 = IdentifierId::new(1);
        let vid = state.initialize(AbstractValue::new(ValueKind::Mutable, ValueReason::Other));
        state.define(id1, vid);
        let place = Place {
            identifier: Identifier {
                id: id1,
                declaration_id: DeclarationId::new(1),
                name: None,
                mutable_range: MutableRange {
                    start: InstructionId::new(0),
                    end: InstructionId::new(0),
                },
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        };

        assert_eq!(
            state.mutate(MutationVariant::Mutate, &place),
            MutationResult::Mutate
        );
        assert_eq!(
            state.mutate(MutationVariant::MutateConditionally, &place),
            MutationResult::Mutate
        );

        // Freeze it
        state.freeze(id1, ValueReason::JsxCaptured);
        assert_eq!(
            state.mutate(MutationVariant::Mutate, &place),
            MutationResult::MutateFrozen
        );
        assert_eq!(
            state.mutate(MutationVariant::MutateConditionally, &place),
            MutationResult::None
        );
    }
}
