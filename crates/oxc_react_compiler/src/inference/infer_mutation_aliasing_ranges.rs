//! Infer mutable ranges for identifiers based on aliasing effects.
//!
//! Port of `InferMutationAliasingRanges.ts` from upstream React Compiler
//! (babel-plugin-react-compiler). Copyright (c) Meta Platforms, Inc. and affiliates.
//! Licensed under MIT.
//!
//! This pass builds an abstract model of the heap and interprets the effects of the
//! given function in order to determine the following:
//! - The mutable ranges of all identifiers in the function
//! - The legacy `Effect` to store on each Place
//!
//! It builds a data flow graph using the effects, tracking an abstract notion of "when"
//! each effect occurs relative to the others. It then walks each mutation effect against
//! the graph, updating the range of each node that would be reachable at the "time"
//! that the effect occurred.

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;
use crate::hir::visitors::map_terminal_operands;
use crate::inference::aliasing_effects::{AliasingEffect, MutationReason};

// ---------------------------------------------------------------------------
// MutationKind
// ---------------------------------------------------------------------------

/// Strength of a mutation: None < Conditional < Definite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MutationKind {
    None = 0,
    Conditional = 1,
    Definite = 2,
}

// ---------------------------------------------------------------------------
// NodeValue — what kind of value the node represents
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum NodeValue {
    Object,
    Phi,
    /// A function value. Stores the inner function's aliasing effects so that
    /// when the function is first mutated/rendered we can propagate any
    /// MutateFrozen/MutateGlobal/Impure errors to the parent scope.
    Function {
        effects: Vec<AliasingEffect>,
    },
}

// ---------------------------------------------------------------------------
// MutationInfo — records the strongest mutation seen for a node
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MutationInfo {
    kind: MutationKind,
    #[allow(dead_code)]
    loc: SourceLocation,
}

// ---------------------------------------------------------------------------
// Node — one node in the aliasing graph
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Node {
    /// The identifier this node tracks.
    #[allow(dead_code)]
    id: IdentifierId,
    /// Edges from values that this node was created from (CreateFrom).
    created_from: HashMap<IdentifierId, usize>,
    /// Edges from values captured into this node.
    captures: HashMap<IdentifierId, usize>,
    /// Edges from values aliased into this node.
    aliases: HashMap<IdentifierId, usize>,
    /// Edges from values maybe-aliased into this node.
    maybe_aliases: HashMap<IdentifierId, usize>,
    /// Forward edges (from this node to others).
    edges: Vec<EdgeInfo>,
    /// Strongest transitive mutation seen.
    transitive: Option<MutationInfo>,
    /// Strongest local (non-transitive) mutation seen.
    local: Option<MutationInfo>,
    /// Index of last mutation (for simulated-mutation reachability).
    last_mutated: usize,
    /// Reason for mutation (e.g. AssignCurrentProperty).
    mutation_reason: Option<MutationReason>,
    /// What kind of value this node represents.
    value: NodeValue,
    /// The current mutable range end for this identifier.
    mutable_range_end: u32,
}

#[derive(Debug)]
struct EdgeInfo {
    index: usize,
    node: IdentifierId,
    kind: EdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    Capture,
    Alias,
    MaybeAlias,
}

// ---------------------------------------------------------------------------
// PendingPhiOperand
// ---------------------------------------------------------------------------

struct PendingPhiOperand {
    from_id: IdentifierId,
    into_id: IdentifierId,
    index: usize,
}

// ---------------------------------------------------------------------------
// PendingMutation
// ---------------------------------------------------------------------------

struct PendingMutation {
    index: usize,
    instruction_id: InstructionId,
    transitive: bool,
    kind: MutationKind,
    identifier_id: IdentifierId,
    reason: Option<MutationReason>,
}

struct PendingRender {
    index: usize,
    identifier_id: IdentifierId,
}

struct MutationRequest<'a> {
    index: usize,
    start: IdentifierId,
    end: Option<InstructionId>,
    transitive: bool,
    start_kind: MutationKind,
    loc: &'a SourceLocation,
    reason: Option<MutationReason>,
}

// ---------------------------------------------------------------------------
// AliasingState
// ---------------------------------------------------------------------------

struct AliasingState {
    nodes: HashMap<IdentifierId, Node>,
}

impl AliasingState {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    fn create(&mut self, identifier_id: IdentifierId, value: NodeValue) {
        self.nodes.insert(
            identifier_id,
            Node {
                id: identifier_id,
                created_from: HashMap::new(),
                captures: HashMap::new(),
                aliases: HashMap::new(),
                maybe_aliases: HashMap::new(),
                edges: Vec::new(),
                transitive: None,
                local: None,
                last_mutated: 0,
                mutation_reason: None,
                value,
                mutable_range_end: 0,
            },
        );
    }

    fn ensure_object(&mut self, identifier_id: IdentifierId) {
        if !self.nodes.contains_key(&identifier_id) {
            self.create(identifier_id, NodeValue::Object);
        }
    }

    fn create_from(&mut self, index: usize, from_id: IdentifierId, into_id: IdentifierId) {
        self.create(into_id, NodeValue::Object);
        // Upstream requires both `from` and `into` nodes to exist.
        if !self.nodes.contains_key(&from_id) || !self.nodes.contains_key(&into_id) {
            return;
        }
        if let Some(from_node) = self.nodes.get_mut(&from_id) {
            from_node.edges.push(EdgeInfo {
                index,
                node: into_id,
                kind: EdgeKind::Alias,
            });
        }
        if let Some(to_node) = self.nodes.get_mut(&into_id) {
            to_node.created_from.entry(from_id).or_insert(index);
        }
    }

    fn capture(&mut self, index: usize, from_id: IdentifierId, into_id: IdentifierId) {
        // Upstream requires both `from` and `into` nodes to exist.
        if !self.nodes.contains_key(&from_id) || !self.nodes.contains_key(&into_id) {
            return;
        }
        if let Some(from_node) = self.nodes.get_mut(&from_id) {
            from_node.edges.push(EdgeInfo {
                index,
                node: into_id,
                kind: EdgeKind::Capture,
            });
        }
        if let Some(to_node) = self.nodes.get_mut(&into_id) {
            to_node.captures.entry(from_id).or_insert(index);
        }
    }

    fn assign(&mut self, index: usize, from_id: IdentifierId, into_id: IdentifierId) {
        // Upstream requires both `from` and `into` nodes to exist.
        if !self.nodes.contains_key(&from_id) || !self.nodes.contains_key(&into_id) {
            return;
        }
        if let Some(from_node) = self.nodes.get_mut(&from_id) {
            from_node.edges.push(EdgeInfo {
                index,
                node: into_id,
                kind: EdgeKind::Alias,
            });
        }
        if let Some(to_node) = self.nodes.get_mut(&into_id) {
            to_node.aliases.entry(from_id).or_insert(index);
        }
    }

    fn maybe_alias(&mut self, index: usize, from_id: IdentifierId, into_id: IdentifierId) {
        // Upstream requires both `from` and `into` nodes to exist.
        if !self.nodes.contains_key(&from_id) || !self.nodes.contains_key(&into_id) {
            return;
        }
        if let Some(from_node) = self.nodes.get_mut(&from_id) {
            from_node.edges.push(EdgeInfo {
                index,
                node: into_id,
                kind: EdgeKind::MaybeAlias,
            });
        }
        if let Some(to_node) = self.nodes.get_mut(&into_id) {
            to_node.maybe_aliases.entry(from_id).or_insert(index);
        }
    }

    /// Propagate a mutation through the aliasing graph.
    ///
    /// `end` is the InstructionId+1 to use for extending mutable ranges.
    /// When `end` is None, this is a simulated mutation (Part 3) that only
    /// records reachability via `last_mutated` but does not extend ranges.
    fn mutate(
        &mut self,
        request: MutationRequest<'_>,
        errors: &mut Vec<crate::error::CompilerDiagnostic>,
    ) {
        let MutationRequest {
            index,
            start,
            end,
            transitive,
            start_kind,
            loc,
            reason,
        } = request;
        let debug_mutate = std::env::var("DEBUG_RANGES_MUTATE_TRACE").is_ok();
        if debug_mutate {
            eprintln!(
                "[RANGES_MUTATE_BEGIN] index={} start={} end={:?} transitive={} kind={:?} reason={:?}",
                index,
                start.0,
                end.map(|id| id.0),
                transitive,
                start_kind,
                reason
            );
        }
        struct QueueEntry {
            place: IdentifierId,
            transitive: bool,
            direction: Direction,
            kind: MutationKind,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Direction {
            Backwards,
            Forwards,
        }

        let mut seen: HashMap<IdentifierId, MutationKind> = HashMap::new();
        let mut queue: Vec<QueueEntry> = vec![QueueEntry {
            place: start,
            transitive,
            direction: Direction::Backwards,
            kind: start_kind,
        }];

        while let Some(entry) = queue.pop() {
            let current = entry.place;
            let transitive = entry.transitive;
            let direction = entry.direction;
            let kind = entry.kind;

            if let Some(&previous_kind) = seen.get(&current)
                && previous_kind >= kind
            {
                continue;
            }
            seen.insert(current, kind);

            // We need to gather info from the node, then update it.
            // Because of borrow checker constraints we do this in steps.

            // First, check if node exists and gather edge info
            let node_info = {
                let node = match self.nodes.get(&current) {
                    Some(n) => n,
                    None => continue,
                };

                // Gather the info we need
                let mut edges: Vec<(usize, IdentifierId, EdgeKind)> = node
                    .edges
                    .iter()
                    .map(|e| (e.index, e.node, e.kind))
                    .collect();
                // Upstream relies on insertion order (effect index order).
                // Keep explicit ordering by index to preserve the `break` guard
                // (`edge.index >= index`) semantics below.
                edges.sort_by_key(|(edge_index, edge_node, _)| (*edge_index, edge_node.0));
                let mut created_from: Vec<(IdentifierId, usize)> =
                    node.created_from.iter().map(|(&k, &v)| (k, v)).collect();
                let mut aliases: Vec<(IdentifierId, usize)> =
                    node.aliases.iter().map(|(&k, &v)| (k, v)).collect();
                let mut maybe_aliases: Vec<(IdentifierId, usize)> =
                    node.maybe_aliases.iter().map(|(&k, &v)| (k, v)).collect();
                let mut captures: Vec<(IdentifierId, usize)> =
                    node.captures.iter().map(|(&k, &v)| (k, v)).collect();
                // Upstream iteration uses Map insertion order (effect index order).
                // HashMap iteration is nondeterministic and can change mutation
                // propagation outcomes because `seen` short-circuits by kind.
                // Sort by edge index to restore deterministic upstream-like flow.
                created_from.sort_by_key(|(id, when)| (*when, id.0));
                aliases.sort_by_key(|(id, when)| (*when, id.0));
                maybe_aliases.sort_by_key(|(id, when)| (*when, id.0));
                captures.sort_by_key(|(id, when)| (*when, id.0));
                let is_phi = matches!(node.value, NodeValue::Phi);

                (
                    edges,
                    created_from,
                    aliases,
                    maybe_aliases,
                    captures,
                    is_phi,
                )
            };

            // Now update the node
            {
                let node = self.nodes.get_mut(&current).unwrap();
                if node.mutation_reason.is_none() {
                    node.mutation_reason = reason;
                }
                node.last_mutated = node.last_mutated.max(index);
                // Propagate inner function errors before this node gets marked as mutated
                if let NodeValue::Function { ref effects } = node.value
                    && node.transitive.is_none()
                    && node.local.is_none()
                {
                    append_function_errors(errors, effects);
                }
                if let Some(end_id) = end {
                    let previous_end = node.mutable_range_end;
                    node.mutable_range_end = node.mutable_range_end.max(end_id.0);
                    if debug_mutate && node.mutable_range_end != previous_end {
                        eprintln!(
                            "[RANGES_MUTATE_NODE] index={} node={} prev_end={} next_end={} transitive={} kind={:?} direction={:?}",
                            index,
                            current.0,
                            previous_end,
                            node.mutable_range_end,
                            transitive,
                            kind,
                            direction
                        );
                    }
                }

                if transitive {
                    match &node.transitive {
                        None => {
                            node.transitive = Some(MutationInfo {
                                kind,
                                loc: loc.clone(),
                            });
                        }
                        Some(existing) if existing.kind < kind => {
                            node.transitive = Some(MutationInfo {
                                kind,
                                loc: loc.clone(),
                            });
                        }
                        _ => {}
                    }
                } else {
                    match &node.local {
                        None => {
                            node.local = Some(MutationInfo {
                                kind,
                                loc: loc.clone(),
                            });
                        }
                        Some(existing) if existing.kind < kind => {
                            node.local = Some(MutationInfo {
                                kind,
                                loc: loc.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }

            let (edges, created_from, aliases, maybe_aliases, captures, is_phi) = node_info;

            // All mutations affect "forward" edges:
            // Capture a -> b, mutate(a) => mutate(b)
            // Alias a -> b, mutate(a) => mutate(b)
            for (edge_index, edge_node, edge_kind) in &edges {
                if *edge_index >= index {
                    break;
                }
                queue.push(QueueEntry {
                    place: *edge_node,
                    transitive,
                    direction: Direction::Forwards,
                    // Traversing a maybeAlias edge always downgrades to conditional mutation
                    kind: if *edge_kind == EdgeKind::MaybeAlias {
                        MutationKind::Conditional
                    } else {
                        kind
                    },
                });
            }

            // createdFrom edges always go backwards
            for (alias, when) in &created_from {
                if *when >= index {
                    continue;
                }
                queue.push(QueueEntry {
                    place: *alias,
                    transitive: true,
                    direction: Direction::Backwards,
                    kind,
                });
            }

            if direction == Direction::Backwards || !is_phi {
                // All mutations affect backward alias edges:
                // Alias a -> b, mutate(b) => mutate(a)
                // However, if we reached a phi because one of its inputs was mutated
                // (advancing "forwards"), the phi's other inputs can't be affected.
                for (alias, when) in &aliases {
                    if *when >= index {
                        continue;
                    }
                    queue.push(QueueEntry {
                        place: *alias,
                        transitive,
                        direction: Direction::Backwards,
                        kind,
                    });
                }
                // MaybeAlias: downgrade to conditional
                for (alias, when) in &maybe_aliases {
                    if *when >= index {
                        continue;
                    }
                    queue.push(QueueEntry {
                        place: *alias,
                        transitive,
                        direction: Direction::Backwards,
                        kind: MutationKind::Conditional,
                    });
                }
            }

            // Only transitive mutations affect captures
            if transitive {
                for (capture, when) in &captures {
                    if *when >= index {
                        continue;
                    }
                    queue.push(QueueEntry {
                        place: *capture,
                        transitive,
                        direction: Direction::Backwards,
                        kind,
                    });
                }
            }
        }
        if debug_mutate {
            eprintln!("[RANGES_MUTATE_END] index={}", index);
        }
    }

    /// Propagate render effects through the aliasing graph.
    #[allow(dead_code)]
    fn render(
        &self,
        index: usize,
        start: IdentifierId,
        errors: &mut Vec<crate::error::CompilerDiagnostic>,
    ) {
        let mut seen: HashSet<IdentifierId> = HashSet::new();
        let mut queue: Vec<IdentifierId> = vec![start];
        while let Some(current) = queue.pop() {
            if seen.contains(&current) {
                continue;
            }
            seen.insert(current);
            let node = match self.nodes.get(&current) {
                Some(n) => n,
                None => continue,
            };
            if node.transitive.is_some() || node.local.is_some() {
                continue;
            }
            // Propagate inner function errors for unmutated Function nodes
            if let NodeValue::Function { ref effects } = node.value {
                append_function_errors(errors, effects);
            }
            let mut created_from: Vec<(IdentifierId, usize)> =
                node.created_from.iter().map(|(&k, &v)| (k, v)).collect();
            let mut aliases: Vec<(IdentifierId, usize)> =
                node.aliases.iter().map(|(&k, &v)| (k, v)).collect();
            let mut captures: Vec<(IdentifierId, usize)> =
                node.captures.iter().map(|(&k, &v)| (k, v)).collect();
            created_from.sort_by_key(|(id, when)| (*when, id.0));
            aliases.sort_by_key(|(id, when)| (*when, id.0));
            captures.sort_by_key(|(id, when)| (*when, id.0));

            for (alias, when) in created_from {
                if when >= index {
                    continue;
                }
                queue.push(alias);
            }
            for (alias, when) in aliases {
                if when >= index {
                    continue;
                }
                queue.push(alias);
            }
            for (capture, when) in captures {
                if when >= index {
                    continue;
                }
                queue.push(capture);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// appendFunctionErrors — propagate inner function errors to parent
// ---------------------------------------------------------------------------

/// Port of upstream `appendFunctionErrors`. When a Function node is first
/// reached during mutation/render traversal, propagate any Impure/MutateFrozen/
/// MutateGlobal errors from its inner aliasing effects to the parent scope.
fn append_function_errors(
    errors: &mut Vec<crate::error::CompilerDiagnostic>,
    effects: &[AliasingEffect],
) {
    for effect in effects {
        match effect {
            AliasingEffect::Impure { error, .. }
            | AliasingEffect::MutateFrozen { error, .. }
            | AliasingEffect::MutateGlobal { error, .. } => {
                errors.push(error.clone());
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: extract IdentifierId from a Place or Argument
// ---------------------------------------------------------------------------

fn place_from_argument(arg: &Argument) -> &Place {
    match arg {
        Argument::Place(p) | Argument::Spread(p) => p,
    }
}

fn is_call_like(value: &InstructionValue) -> bool {
    matches!(
        value,
        InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
    )
}

fn has_handler_alias(instr: &Instruction, handler_id: IdentifierId) -> bool {
    instr.effects.as_ref().is_some_and(|effects| {
        effects.iter().any(|effect| {
            matches!(
                effect,
                AliasingEffect::Alias { into, .. } if into.identifier.id == handler_id
            )
        })
    })
}

fn append_node_mutation_effects(
    place: &mut Place,
    state: &AliasingState,
    function_effects: &mut Vec<AliasingEffect>,
) {
    let Some(node) = state.nodes.get(&place.identifier.id) else {
        return;
    };

    let mut mutated = false;
    if let Some(local) = &node.local {
        let mut value = place.clone();
        value.loc = local.loc.clone();
        match local.kind {
            MutationKind::None => {}
            MutationKind::Conditional => {
                mutated = true;
                function_effects.push(AliasingEffect::MutateConditionally { value });
            }
            MutationKind::Definite => {
                mutated = true;
                function_effects.push(AliasingEffect::Mutate {
                    value,
                    reason: node.mutation_reason,
                });
            }
        }
    }
    if let Some(transitive) = &node.transitive {
        let mut value = place.clone();
        value.loc = transitive.loc.clone();
        match transitive.kind {
            MutationKind::None => {}
            MutationKind::Conditional => {
                mutated = true;
                function_effects.push(AliasingEffect::MutateTransitiveConditionally { value });
            }
            MutationKind::Definite => {
                mutated = true;
                function_effects.push(AliasingEffect::MutateTransitive { value });
            }
        }
    }
    if mutated {
        place.effect = Effect::Capture;
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Infer mutable ranges for all identifiers in the function.
///
/// This pass:
/// 1. Builds an aliasing graph from instruction effects
/// 2. Propagates mutations through the graph to extend mutable ranges
/// 3. Computes function-level aliasing effects (Part 3: simulated transitive mutations)
/// 4. Writes the computed mutable ranges back onto identifiers
/// 5. Assigns legacy `Effect` values to instruction operands and lvalues
///
/// Returns the function-level aliasing effects for use by `analyse_functions`.
///
/// `is_function_expression` controls whether the return terminal uses
/// `Effect::Read` (for function expressions) or `Effect::Freeze`.
pub fn infer_mutation_aliasing_ranges(
    func: &mut HIRFunction,
    is_function_expression: bool,
) -> Result<Vec<AliasingEffect>, crate::error::CompilerError> {
    let debug_ranges = std::env::var("DEBUG_INFER_MUTATION_ALIASING_RANGES").is_ok();

    fn summarize_effect(effect: &AliasingEffect) -> String {
        match effect {
            AliasingEffect::Assign { from, into } => {
                format!("Assign({}->{})", from.identifier.id.0, into.identifier.id.0)
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
            AliasingEffect::Capture { from, into } => {
                format!(
                    "Capture({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::Create { into, .. } => format!("Create({})", into.identifier.id.0),
            AliasingEffect::CreateFrom { from, into } => {
                format!(
                    "CreateFrom({}->{})",
                    from.identifier.id.0, into.identifier.id.0
                )
            }
            AliasingEffect::CreateFunction { into, .. } => {
                format!("CreateFunction({})", into.identifier.id.0)
            }
            AliasingEffect::Mutate { value, .. } => format!("Mutate({})", value.identifier.id.0),
            AliasingEffect::MutateConditionally { value } => {
                format!("MutateConditionally({})", value.identifier.id.0)
            }
            AliasingEffect::MutateTransitive { value } => {
                format!("MutateTransitive({})", value.identifier.id.0)
            }
            AliasingEffect::MutateTransitiveConditionally { value } => {
                format!("MutateTransitiveConditionally({})", value.identifier.id.0)
            }
            AliasingEffect::Freeze { value, .. } => format!("Freeze({})", value.identifier.id.0),
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
            AliasingEffect::Impure { place, .. } => format!("Impure({})", place.identifier.id.0),
            AliasingEffect::Render { place } => format!("Render({})", place.identifier.id.0),
        }
    }

    // -----------------------------------------------------------------------
    // Part 1: Build the aliasing graph and collect mutations
    // -----------------------------------------------------------------------
    let mut state = AliasingState::new();
    let mut pending_phis: HashMap<BlockId, Vec<PendingPhiOperand>> = HashMap::new();
    let mut mutations: Vec<PendingMutation> = Vec::new();
    let mut renders: Vec<PendingRender> = Vec::new();
    let mut index: usize = 0;

    // Track catch-handler bindings by handler block. Upstream models this as
    // terminal.effects on MaybeThrow; our HIR terminals don't carry effects.
    let catch_bindings: HashMap<BlockId, Place> = func
        .body
        .blocks
        .iter()
        .filter_map(|(_, block)| match &block.terminal {
            Terminal::Try {
                handler_binding: Some(binding),
                handler,
                ..
            } => Some((*handler, binding.clone())),
            _ => None,
        })
        .collect();

    // Create nodes for params, context vars, and the return place
    for param in &func.params {
        let place = place_from_argument(param);
        state.create(place.identifier.id, NodeValue::Object);
    }
    for ctx_place in &func.context {
        state.create(ctx_place.identifier.id, NodeValue::Object);
    }
    state.create(func.returns.identifier.id, NodeValue::Object);

    let mut seen_blocks: HashSet<BlockId> = HashSet::new();

    // First pass: iterate blocks, build graph edges, collect mutations
    // We need to iterate blocks by index since we can't borrow mutably
    let block_count = func.body.blocks.len();
    for block_idx in 0..block_count {
        let block_id = func.body.blocks[block_idx].0;

        // Process phis
        let phi_count = func.body.blocks[block_idx].1.phis.len();
        for phi_idx in 0..phi_count {
            let phi_place_id = func.body.blocks[block_idx].1.phis[phi_idx]
                .place
                .identifier
                .id;
            state.create(phi_place_id, NodeValue::Phi);

            // Collect operands info
            let mut operands: Vec<(BlockId, IdentifierId)> = func.body.blocks[block_idx].1.phis
                [phi_idx]
                .operands
                .iter()
                .map(|(&pred, op)| (pred, op.identifier.id))
                .collect();
            // Phi operands are ordered in upstream by predecessor insertion.
            // HashMap iteration here is nondeterministic; stabilize by block id.
            operands.sort_by_key(|(pred, operand_id)| (pred.0, operand_id.0));

            for (pred, operand_id) in operands {
                if !seen_blocks.contains(&pred) {
                    let block_phis = pending_phis.entry(pred).or_default();
                    block_phis.push(PendingPhiOperand {
                        from_id: operand_id,
                        into_id: phi_place_id,
                        index,
                    });
                    index += 1;
                } else {
                    state.assign(index, operand_id, phi_place_id);
                    index += 1;
                }
            }
        }
        seen_blocks.insert(block_id);

        // Process instructions
        let instr_count = func.body.blocks[block_idx].1.instructions.len();
        for instr_idx in 0..instr_count {
            let instr = &func.body.blocks[block_idx].1.instructions[instr_idx];
            let instr_id = instr.id;

            if let Some(ref effects) = instr.effects {
                for effect in effects {
                    match effect {
                        AliasingEffect::Create { into, .. } => {
                            state.create(into.identifier.id, NodeValue::Object);
                        }
                        AliasingEffect::CreateFunction { into, .. } => {
                            // Extract inner function's aliasing effects from the
                            // instruction value (FunctionExpression/ObjectMethod).
                            let inner_effects = match &instr.value {
                                InstructionValue::FunctionExpression { lowered_func, .. }
                                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                                    lowered_func
                                        .func
                                        .aliasing_effects
                                        .clone()
                                        .unwrap_or_default()
                                }
                                _ => Vec::new(),
                            };
                            state.create(
                                into.identifier.id,
                                NodeValue::Function {
                                    effects: inner_effects,
                                },
                            );
                        }
                        AliasingEffect::CreateFrom { from, into } => {
                            state.ensure_object(from.identifier.id);
                            state.create_from(index, from.identifier.id, into.identifier.id);
                            index += 1;
                        }
                        AliasingEffect::Assign { from, into } => {
                            state.ensure_object(from.identifier.id);
                            state.ensure_object(into.identifier.id);
                            state.assign(index, from.identifier.id, into.identifier.id);
                            index += 1;
                        }
                        AliasingEffect::Alias { from, into } => {
                            state.ensure_object(from.identifier.id);
                            state.ensure_object(into.identifier.id);
                            state.assign(index, from.identifier.id, into.identifier.id);
                            index += 1;
                        }
                        AliasingEffect::MaybeAlias { from, into } => {
                            state.ensure_object(from.identifier.id);
                            state.ensure_object(into.identifier.id);
                            state.maybe_alias(index, from.identifier.id, into.identifier.id);
                            index += 1;
                        }
                        AliasingEffect::Capture { from, into } => {
                            state.ensure_object(from.identifier.id);
                            state.ensure_object(into.identifier.id);
                            state.capture(index, from.identifier.id, into.identifier.id);
                            index += 1;
                        }
                        AliasingEffect::MutateTransitive { value }
                        | AliasingEffect::MutateTransitiveConditionally { value } => {
                            state.ensure_object(value.identifier.id);
                            let kind = if matches!(effect, AliasingEffect::MutateTransitive { .. })
                            {
                                MutationKind::Definite
                            } else {
                                MutationKind::Conditional
                            };
                            mutations.push(PendingMutation {
                                index,
                                instruction_id: instr_id,
                                transitive: true,
                                kind,
                                identifier_id: value.identifier.id,
                                reason: None,
                            });
                            index += 1;
                        }
                        AliasingEffect::Mutate { value, reason } => {
                            state.ensure_object(value.identifier.id);
                            mutations.push(PendingMutation {
                                index,
                                instruction_id: instr_id,
                                transitive: false,
                                kind: MutationKind::Definite,
                                identifier_id: value.identifier.id,
                                reason: *reason,
                            });
                            index += 1;
                        }
                        AliasingEffect::MutateConditionally { value } => {
                            state.ensure_object(value.identifier.id);
                            mutations.push(PendingMutation {
                                index,
                                instruction_id: instr_id,
                                transitive: false,
                                kind: MutationKind::Conditional,
                                identifier_id: value.identifier.id,
                                reason: None,
                            });
                            index += 1;
                        }
                        AliasingEffect::MutateFrozen { .. }
                        | AliasingEffect::MutateGlobal { .. }
                        | AliasingEffect::Impure { .. } => {
                            // These are error effects; we collect them for the function-level
                            // effects but skip them here for range inference.
                        }
                        AliasingEffect::Render { place } => {
                            state.ensure_object(place.identifier.id);
                            renders.push(PendingRender {
                                index,
                                identifier_id: place.identifier.id,
                            });
                            index += 1;
                        }
                        AliasingEffect::Freeze { .. }
                        | AliasingEffect::ImmutableCapture { .. }
                        | AliasingEffect::Apply { .. } => {
                            // These effects don't create graph edges or mutations
                            // Apply should have been resolved by infer_mutation_aliasing_effects
                        }
                    }
                }
            }
        }

        // Process pending phi operands for this block
        if let Some(block_phis) = pending_phis.get(&block_id) {
            for phi in block_phis {
                state.assign(phi.index, phi.from_id, phi.into_id);
            }
        }

        // Process return terminal: assign value into fn.returns
        let terminal = &func.body.blocks[block_idx].1.terminal;
        if let Terminal::Return { value, .. } = terminal {
            let returns_id = func.returns.identifier.id;
            state.assign(index, value.identifier.id, returns_id);
            index += 1;
        }

        // Approximate upstream terminal.effects for MaybeThrow by synthesizing
        // catch aliases for call-like instruction results in this block.
        if let Terminal::MaybeThrow { handler, .. } = terminal
            && let Some(binding) = catch_bindings.get(handler)
        {
            let handler_id = binding.identifier.id;
            for instr in &func.body.blocks[block_idx].1.instructions {
                if !is_call_like(&instr.value) || has_handler_alias(instr, handler_id) {
                    continue;
                }
                state.assign(index, instr.lvalue.identifier.id, handler_id);
                index += 1;
            }
        }
    }

    // Error diagnostics collected from mutation/render propagation and inner functions.
    let mut error_diagnostics: Vec<crate::error::CompilerDiagnostic> = Vec::new();

    // Apply mutations
    for mutation in &mutations {
        let end = make_instruction_id(mutation.instruction_id.0 + 1);
        // We need a SourceLocation for the mutation. Since we don't store it on the
        // PendingMutation, use Generated. The upstream uses `mutation.place.loc`.
        let loc = SourceLocation::Generated;
        state.mutate(
            MutationRequest {
                index: mutation.index,
                start: mutation.identifier_id,
                end: Some(end),
                transitive: mutation.transitive,
                start_kind: mutation.kind,
                loc: &loc,
                reason: mutation.reason,
            },
            &mut error_diagnostics,
        );
    }

    // Apply renders
    for render in &renders {
        state.render(render.index, render.identifier_id, &mut error_diagnostics);
    }

    // -----------------------------------------------------------------------
    // Part 3: Compute function-level aliasing effects via simulated mutations
    // -----------------------------------------------------------------------
    // Determine precise data-flow effects by simulating transitive mutations
    // of the params/captures and seeing what other params/context variables
    // are affected. Anything that would be transitively mutated needs a
    // capture relationship.
    let mut function_effects: Vec<AliasingEffect> = Vec::new();
    // Collect error/render effects that bubble up.
    for block_idx in 0..func.body.blocks.len() {
        for instr in &func.body.blocks[block_idx].1.instructions {
            if let Some(ref effects) = instr.effects {
                for effect in effects {
                    match effect {
                        AliasingEffect::MutateFrozen { error, .. }
                        | AliasingEffect::MutateGlobal { error, .. }
                        | AliasingEffect::Impure { error, .. } => {
                            error_diagnostics.push(error.clone());
                            function_effects.push(effect.clone());
                        }
                        AliasingEffect::Render { .. } => {
                            function_effects.push(effect.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Bubble-up mutations on context vars and params to function effects.
    for ctx_place in &mut func.context {
        append_node_mutation_effects(ctx_place, &state, &mut function_effects);
    }
    for param in &mut func.params {
        let place = match param {
            Argument::Place(place) | Argument::Spread(place) => place,
        };
        append_node_mutation_effects(place, &state, &mut function_effects);
    }

    // Create return-value Create effect
    {
        let returns_type = &func.returns.identifier.type_;
        let value_kind = if matches!(returns_type, Type::Primitive) {
            ValueKind::Primitive
        } else {
            // Upstream checks isJsxType for Frozen; we approximate with Mutable
            // since we don't have a reliable JSX type check here
            ValueKind::Mutable
        };
        function_effects.push(AliasingEffect::Create {
            into: func.returns.clone(),
            value: value_kind,
            reason: ValueReason::KnownReturnSignature,
        });
    }

    // Collect tracked values: params, context vars, returns
    let tracked: Vec<Place> = func
        .params
        .iter()
        .map(|arg| match arg {
            Argument::Place(p) | Argument::Spread(p) => p.clone(),
        })
        .chain(func.context.iter().cloned())
        .chain(std::iter::once(func.returns.clone()))
        .collect();

    // For each tracked value, simulate a conditional transitive mutation
    // and check which other tracked values are affected
    // Use ignored_errors for simulated mutations (matches upstream pattern).
    let mut ignored_errors: Vec<crate::error::CompilerDiagnostic> = Vec::new();
    let returns_id = func.returns.identifier.id;
    for into in &tracked {
        let mutation_index = index;
        index += 1;
        state.mutate(
            MutationRequest {
                index: mutation_index,
                start: into.identifier.id,
                end: None, // simulated mutation — no range extension
                transitive: true,
                start_kind: MutationKind::Conditional,
                loc: &into.loc,
                reason: None,
            },
            &mut ignored_errors,
        );
        for from in &tracked {
            if from.identifier.id == into.identifier.id {
                continue; // skip self
            }
            if from.identifier.id == returns_id {
                continue; // skip from if it's returns
            }
            if let Some(from_node) = state.nodes.get(&from.identifier.id)
                && from_node.last_mutated == mutation_index
            {
                if into.identifier.id == returns_id {
                    // Return value could be any of the params/context variables
                    function_effects.push(AliasingEffect::Alias {
                        from: from.clone(),
                        into: into.clone(),
                    });
                } else {
                    // Params/context-vars can only capture each other
                    function_effects.push(AliasingEffect::Capture {
                        from: from.clone(),
                        into: into.clone(),
                    });
                }
            }
        }
    }

    // Write mutable range ends back to the function's identifiers.
    // We need to collect the computed end values from the state first.
    let range_ends: HashMap<IdentifierId, u32> = state
        .nodes
        .iter()
        .filter(|(_, node)| node.mutable_range_end > 0)
        .map(|(&id, node)| (id, node.mutable_range_end))
        .collect();

    // -----------------------------------------------------------------------
    // Part 2: Assign legacy effects and fix up mutable ranges
    // -----------------------------------------------------------------------

    // Helper: apply the range ends computed in Part 1
    fn apply_range_end(identifier: &mut Identifier, range_ends: &HashMap<IdentifierId, u32>) {
        if let Some(&end) = range_ends.get(&identifier.id)
            && end > identifier.mutable_range.end.0
        {
            identifier.mutable_range.end = InstructionId(end);
        }
    }

    for block_idx in 0..func.body.blocks.len() {
        // Fix up phis
        let phi_count = func.body.blocks[block_idx].1.phis.len();
        for phi_idx in 0..phi_count {
            // Apply the range end from Part 1 to the phi's place
            apply_range_end(
                &mut func.body.blocks[block_idx].1.phis[phi_idx].place.identifier,
                &range_ends,
            );

            // Determine isPhiMutatedAfterCreation
            let phi_range_end = func.body.blocks[block_idx].1.phis[phi_idx]
                .place
                .identifier
                .mutable_range
                .end;
            let first_instr_id = func.body.blocks[block_idx]
                .1
                .instructions
                .first()
                .map(|i| i.id)
                .unwrap_or_else(|| func.body.blocks[block_idx].1.terminal.id());
            let is_phi_mutated_after_creation = phi_range_end.0 > first_instr_id.0;

            // Set phi place effect
            func.body.blocks[block_idx].1.phis[phi_idx].place.effect = Effect::Store;

            // Set operand effects
            let operand_effect = if is_phi_mutated_after_creation {
                Effect::Capture
            } else {
                Effect::Read
            };
            for operand in func.body.blocks[block_idx].1.phis[phi_idx]
                .operands
                .values_mut()
            {
                operand.effect = operand_effect;
            }

            // Fix phi mutable range start if needed
            if is_phi_mutated_after_creation
                && func.body.blocks[block_idx].1.phis[phi_idx]
                    .place
                    .identifier
                    .mutable_range
                    .start
                    .0
                    == 0
            {
                let start_id = if first_instr_id.0 > 0 {
                    first_instr_id.0 - 1
                } else {
                    0
                };
                func.body.blocks[block_idx].1.phis[phi_idx]
                    .place
                    .identifier
                    .mutable_range
                    .start = InstructionId(start_id);
            }
        }

        // Process instructions
        let instr_count = func.body.blocks[block_idx].1.instructions.len();
        for instr_idx in 0..instr_count {
            let instr_id = func.body.blocks[block_idx].1.instructions[instr_idx].id;

            // Apply range ends from Part 1 to all lvalue identifiers
            // We need to do this through the lvalue and instruction value's lvalues
            {
                let instr = &mut func.body.blocks[block_idx].1.instructions[instr_idx];
                apply_range_end(&mut instr.lvalue.identifier, &range_ends);

                // Apply to nested lvalues
                match &mut instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                    | InstructionValue::StoreContext { lvalue, .. }
                    | InstructionValue::DeclareLocal { lvalue, .. }
                    | InstructionValue::DeclareContext { lvalue, .. } => {
                        apply_range_end(&mut lvalue.place.identifier, &range_ends);
                    }
                    InstructionValue::Destructure { lvalue, .. } => {
                        apply_range_end_pattern(&mut lvalue.pattern, &range_ends);
                    }
                    InstructionValue::PrefixUpdate { lvalue, .. }
                    | InstructionValue::PostfixUpdate { lvalue, .. } => {
                        apply_range_end(&mut lvalue.identifier, &range_ends);
                    }
                    _ => {}
                }
            }

            // Set default lvalue effects and fix ranges
            {
                let instr = &mut func.body.blocks[block_idx].1.instructions[instr_idx];

                // Default lvalue effect: ConditionallyMutate
                instr.lvalue.effect = Effect::ConditionallyMutate;
                if instr.lvalue.identifier.mutable_range.start.0 == 0 {
                    instr.lvalue.identifier.mutable_range.start = instr_id;
                }
                if instr.lvalue.identifier.mutable_range.end.0 == 0 {
                    let new_end =
                        std::cmp::max(instr_id.0 + 1, instr.lvalue.identifier.mutable_range.end.0);
                    instr.lvalue.identifier.mutable_range.end = InstructionId(new_end);
                }

                // Same for nested lvalues
                match &mut instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                    | InstructionValue::StoreContext { lvalue, .. }
                    | InstructionValue::DeclareLocal { lvalue, .. }
                    | InstructionValue::DeclareContext { lvalue, .. } => {
                        lvalue.place.effect = Effect::ConditionallyMutate;
                        if lvalue.place.identifier.mutable_range.start.0 == 0 {
                            lvalue.place.identifier.mutable_range.start = instr_id;
                        }
                        if lvalue.place.identifier.mutable_range.end.0 == 0 {
                            let new_end = std::cmp::max(
                                instr_id.0 + 1,
                                lvalue.place.identifier.mutable_range.end.0,
                            );
                            lvalue.place.identifier.mutable_range.end = InstructionId(new_end);
                        }
                    }
                    InstructionValue::Destructure { lvalue, .. } => {
                        set_default_lvalue_effects_pattern(&mut lvalue.pattern, instr_id);
                    }
                    InstructionValue::PrefixUpdate { lvalue, .. }
                    | InstructionValue::PostfixUpdate { lvalue, .. } => {
                        lvalue.effect = Effect::ConditionallyMutate;
                        if lvalue.identifier.mutable_range.start.0 == 0 {
                            lvalue.identifier.mutable_range.start = instr_id;
                        }
                        if lvalue.identifier.mutable_range.end.0 == 0 {
                            let new_end = std::cmp::max(
                                instr_id.0 + 1,
                                lvalue.identifier.mutable_range.end.0,
                            );
                            lvalue.identifier.mutable_range.end = InstructionId(new_end);
                        }
                    }
                    _ => {}
                }
            }

            // Set default operand effects to Read
            {
                let instr = &mut func.body.blocks[block_idx].1.instructions[instr_idx];
                set_value_operand_effects(&mut instr.value, Effect::Read);
            }

            // Now process per-effect operand overrides
            let effects_clone = func.body.blocks[block_idx].1.instructions[instr_idx]
                .effects
                .clone();
            if debug_ranges {
                let instr = &func.body.blocks[block_idx].1.instructions[instr_idx];
                if matches!(
                    instr.value,
                    InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                ) {
                    let summary = effects_clone
                        .as_ref()
                        .map(|effects| {
                            effects
                                .iter()
                                .map(summarize_effect)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_else(|| "<none>".to_string());
                    eprintln!(
                        "[RANGES_EFFECTS] bb{} instr#{} lvalue={} effects=[{}]",
                        func.body.blocks[block_idx].0.0,
                        instr.id.0,
                        instr.lvalue.identifier.id.0,
                        summary
                    );
                }
            }
            if let Some(ref effects) = effects_clone {
                let mut operand_effects: HashMap<IdentifierId, Effect> = HashMap::new();

                for effect in effects {
                    match effect {
                        AliasingEffect::Assign { from, into }
                        | AliasingEffect::Alias { from, into }
                        | AliasingEffect::Capture { from, into }
                        | AliasingEffect::CreateFrom { from, into }
                        | AliasingEffect::MaybeAlias { from, into } => {
                            let is_mutated_or_reassigned =
                                into.identifier.mutable_range.end.0 > instr_id.0;
                            // Check the live range end from Part 1 too
                            let live_end = range_ends
                                .get(&into.identifier.id)
                                .copied()
                                .unwrap_or(into.identifier.mutable_range.end.0);
                            let is_mutated = live_end > instr_id.0 || is_mutated_or_reassigned;

                            if is_mutated {
                                merge_operand_effect(
                                    &mut operand_effects,
                                    from.identifier.id,
                                    Effect::Capture,
                                );
                                merge_operand_effect(
                                    &mut operand_effects,
                                    into.identifier.id,
                                    Effect::Store,
                                );
                            } else {
                                merge_operand_effect(
                                    &mut operand_effects,
                                    from.identifier.id,
                                    Effect::Read,
                                );
                                merge_operand_effect(
                                    &mut operand_effects,
                                    into.identifier.id,
                                    Effect::Store,
                                );
                            }

                            if debug_ranges {
                                let kind = match effect {
                                    AliasingEffect::Assign { .. } => "Assign",
                                    AliasingEffect::Alias { .. } => "Alias",
                                    AliasingEffect::Capture { .. } => "Capture",
                                    AliasingEffect::CreateFrom { .. } => "CreateFrom",
                                    AliasingEffect::MaybeAlias { .. } => "MaybeAlias",
                                    _ => unreachable!(),
                                };
                                eprintln!(
                                    "[RANGES_ALIAS] bb{} instr#{} kind={} from={} into={} into_end={} live_end={} mutated={}",
                                    func.body.blocks[block_idx].0.0,
                                    instr_id.0,
                                    kind,
                                    from.identifier.id.0,
                                    into.identifier.id.0,
                                    into.identifier.mutable_range.end.0,
                                    live_end,
                                    is_mutated
                                );
                            }
                        }
                        AliasingEffect::CreateFunction { .. } | AliasingEffect::Create { .. } => {
                            // no-op
                        }
                        AliasingEffect::Mutate { value, .. } => {
                            merge_operand_effect(
                                &mut operand_effects,
                                value.identifier.id,
                                Effect::Store,
                            );
                        }
                        AliasingEffect::MutateTransitive { value }
                        | AliasingEffect::MutateConditionally { value }
                        | AliasingEffect::MutateTransitiveConditionally { value } => {
                            merge_operand_effect(
                                &mut operand_effects,
                                value.identifier.id,
                                Effect::ConditionallyMutate,
                            );
                        }
                        AliasingEffect::Freeze { value, .. } => {
                            merge_operand_effect(
                                &mut operand_effects,
                                value.identifier.id,
                                Effect::Freeze,
                            );
                        }
                        AliasingEffect::ImmutableCapture { .. } => {
                            // no-op, Read is the default
                        }
                        AliasingEffect::Impure { .. }
                        | AliasingEffect::Render { .. }
                        | AliasingEffect::MutateFrozen { .. }
                        | AliasingEffect::MutateGlobal { .. } => {
                            // no-op
                        }
                        AliasingEffect::Apply { .. } => {
                            // Should have been resolved, but ignore if present
                        }
                    }
                }

                // Apply operand effects to lvalues
                let instr = &mut func.body.blocks[block_idx].1.instructions[instr_idx];
                if let Some(&eff) = operand_effects.get(&instr.lvalue.identifier.id) {
                    instr.lvalue.effect = eff;
                }
                // Apply to nested lvalues
                match &mut instr.value {
                    InstructionValue::StoreLocal { lvalue, .. }
                    | InstructionValue::StoreContext { lvalue, .. }
                    | InstructionValue::DeclareLocal { lvalue, .. }
                    | InstructionValue::DeclareContext { lvalue, .. } => {
                        if let Some(&eff) = operand_effects.get(&lvalue.place.identifier.id) {
                            lvalue.place.effect = eff;
                        }
                    }
                    InstructionValue::Destructure { lvalue, .. } => {
                        apply_operand_effects_pattern(&mut lvalue.pattern, &operand_effects);
                    }
                    InstructionValue::PrefixUpdate { lvalue, .. }
                    | InstructionValue::PostfixUpdate { lvalue, .. } => {
                        if let Some(&eff) = operand_effects.get(&lvalue.identifier.id) {
                            lvalue.effect = eff;
                        }
                    }
                    _ => {}
                }

                // Apply operand effects to value operands and fix up their ranges
                apply_operand_effects_to_value(
                    &mut func.body.blocks[block_idx].1.instructions[instr_idx].value,
                    &operand_effects,
                    &range_ends,
                    instr_id,
                );

                if debug_ranges {
                    let instr = &func.body.blocks[block_idx].1.instructions[instr_idx];
                    match &instr.value {
                        InstructionValue::MethodCall {
                            receiver, property, ..
                        } => {
                            eprintln!(
                                "[RANGES_EFFECTS] bb{} instr#{} method receiver={} effect={:?} property={} effect={:?}",
                                func.body.blocks[block_idx].0.0,
                                instr.id.0,
                                receiver.identifier.id.0,
                                receiver.effect,
                                property.identifier.id.0,
                                property.effect
                            );
                        }
                        InstructionValue::CallExpression { callee, .. } => {
                            eprintln!(
                                "[RANGES_EFFECTS] bb{} instr#{} call callee={} effect={:?}",
                                func.body.blocks[block_idx].0.0,
                                instr.id.0,
                                callee.identifier.id.0,
                                callee.effect
                            );
                        }
                        _ => {}
                    }
                }
            }

            // StoreContext special case: extend rvalue range for hoisted functions
            {
                let instr = &mut func.body.blocks[block_idx].1.instructions[instr_idx];
                if let InstructionValue::StoreContext { value, .. } = &mut instr.value
                    && value.identifier.mutable_range.end.0 <= instr_id.0
                {
                    value.identifier.mutable_range.end = InstructionId(instr_id.0 + 1);
                }
            }
        }

        // Process terminal operand effects
        let terminal = &mut func.body.blocks[block_idx].1.terminal;
        match terminal {
            Terminal::Return { value, .. } => {
                value.effect = if is_function_expression {
                    Effect::Read
                } else {
                    Effect::Freeze
                };
            }
            _ => {
                map_terminal_operands(terminal, |operand| {
                    operand.effect = Effect::Read;
                });
            }
        }
    }

    // Return error if there are validation diagnostics (MutateFrozen, MutateGlobal, Impure)
    // Function expressions propagate errors to parent; top-level functions bail.
    if !error_diagnostics.is_empty() && !is_function_expression {
        return Err(crate::error::CompilerError::Bail(crate::error::BailOut {
            reason: "Mutation validation errors".to_string(),
            diagnostics: error_diagnostics,
        }));
    }

    Ok(function_effects)
}

// ---------------------------------------------------------------------------
// Helpers for Part 2
// ---------------------------------------------------------------------------

fn apply_range_end_pattern(pattern: &mut Pattern, range_ends: &HashMap<IdentifierId, u32>) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        apply_range_end_to_identifier(&mut p.identifier, range_ends);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        apply_range_end_to_identifier(&mut p.place.identifier, range_ends);
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        apply_range_end_to_identifier(&mut p.identifier, range_ends);
                    }
                }
            }
        }
    }
}

fn apply_range_end_to_identifier(
    identifier: &mut Identifier,
    range_ends: &HashMap<IdentifierId, u32>,
) {
    if let Some(&end) = range_ends.get(&identifier.id)
        && end > identifier.mutable_range.end.0
    {
        identifier.mutable_range.end = InstructionId(end);
    }
}

fn effect_precedence(effect: Effect) -> u8 {
    match effect {
        Effect::Unknown => 0,
        Effect::Read => 1,
        Effect::Capture => 2,
        Effect::ConditionallyMutateIterator | Effect::ConditionallyMutate => 3,
        Effect::Mutate => 4,
        Effect::Store => 5,
        Effect::Freeze => 6,
    }
}

fn merge_operand_effect(
    operand_effects: &mut HashMap<IdentifierId, Effect>,
    id: IdentifierId,
    next: Effect,
) {
    operand_effects
        .entry(id)
        .and_modify(|current| {
            if effect_precedence(next) > effect_precedence(*current) {
                *current = next;
            }
        })
        .or_insert(next);
}

fn set_default_lvalue_effects_pattern(pattern: &mut Pattern, instr_id: InstructionId) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        p.effect = Effect::ConditionallyMutate;
                        if p.identifier.mutable_range.start.0 == 0 {
                            p.identifier.mutable_range.start = instr_id;
                        }
                        if p.identifier.mutable_range.end.0 == 0 {
                            let new_end =
                                std::cmp::max(instr_id.0 + 1, p.identifier.mutable_range.end.0);
                            p.identifier.mutable_range.end = InstructionId(new_end);
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        p.place.effect = Effect::ConditionallyMutate;
                        if p.place.identifier.mutable_range.start.0 == 0 {
                            p.place.identifier.mutable_range.start = instr_id;
                        }
                        if p.place.identifier.mutable_range.end.0 == 0 {
                            let new_end = std::cmp::max(
                                instr_id.0 + 1,
                                p.place.identifier.mutable_range.end.0,
                            );
                            p.place.identifier.mutable_range.end = InstructionId(new_end);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        p.effect = Effect::ConditionallyMutate;
                        if p.identifier.mutable_range.start.0 == 0 {
                            p.identifier.mutable_range.start = instr_id;
                        }
                        if p.identifier.mutable_range.end.0 == 0 {
                            let new_end =
                                std::cmp::max(instr_id.0 + 1, p.identifier.mutable_range.end.0);
                            p.identifier.mutable_range.end = InstructionId(new_end);
                        }
                    }
                }
            }
        }
    }
}

fn apply_operand_effects_pattern(
    pattern: &mut Pattern,
    operand_effects: &HashMap<IdentifierId, Effect>,
) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &mut arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        if let Some(&eff) = operand_effects.get(&p.identifier.id) {
                            p.effect = eff;
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &mut obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let Some(&eff) = operand_effects.get(&p.place.identifier.id) {
                            p.place.effect = eff;
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        if let Some(&eff) = operand_effects.get(&p.identifier.id) {
                            p.effect = eff;
                        }
                    }
                }
            }
        }
    }
}

/// Set all operand Places in an instruction value to the given effect.
fn set_value_operand_effects(value: &mut InstructionValue, effect: Effect) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            place.effect = effect;
        }
        InstructionValue::StoreLocal { value: val, .. }
        | InstructionValue::StoreContext { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::Destructure { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            left.effect = effect;
            right.effect = effect;
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            callee.effect = effect;
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => p.effect = effect,
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            receiver.effect = effect;
            property.effect = effect;
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => p.effect = effect,
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            callee.effect = effect;
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => p.effect = effect,
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        p.place.effect = effect;
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            place.effect = effect;
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => p.effect = effect,
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => p.effect = effect,
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
                p.effect = effect;
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => place.effect = effect,
                    JsxAttribute::SpreadAttribute { argument } => argument.effect = effect,
                }
            }
            if let Some(children) = children {
                for child in children {
                    child.effect = effect;
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                child.effect = effect;
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            object.effect = effect;
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            object.effect = effect;
            val.effect = effect;
        }
        InstructionValue::PropertyDelete { object, .. } => {
            object.effect = effect;
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            object.effect = effect;
            property.effect = effect;
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            object.effect = effect;
            property.effect = effect;
            val.effect = effect;
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            object.effect = effect;
            property.effect = effect;
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            tag.effect = effect;
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for s in subexprs {
                s.effect = effect;
            }
        }
        InstructionValue::Await { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::GetIterator { collection, .. } => {
            collection.effect = effect;
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            iterator.effect = effect;
            collection.effect = effect;
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::PrefixUpdate { value: val, .. }
        | InstructionValue::PostfixUpdate { value: val, .. } => {
            val.effect = effect;
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            decl.effect = effect;
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            test.effect = effect;
            consequent.effect = effect;
            alternate.effect = effect;
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            left.effect = effect;
            right.effect = effect;
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            for operand in &mut lowered_func.func.context {
                operand.effect = effect;
            }
        }
        // No operands
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

/// Apply per-operand effects and fix up mutable range starts for value operands.
fn apply_operand_effects_to_value(
    value: &mut InstructionValue,
    operand_effects: &HashMap<IdentifierId, Effect>,
    range_ends: &HashMap<IdentifierId, u32>,
    instr_id: InstructionId,
) {
    let apply = |place: &mut Place| {
        // Fix up mutable range start if needed
        let live_end = range_ends
            .get(&place.identifier.id)
            .copied()
            .unwrap_or(place.identifier.mutable_range.end.0);
        let effective_end = std::cmp::max(live_end, place.identifier.mutable_range.end.0);
        if effective_end > instr_id.0 && place.identifier.mutable_range.start.0 == 0 {
            place.identifier.mutable_range.start = instr_id;
        }

        // Apply range end
        if let Some(&end) = range_ends.get(&place.identifier.id)
            && end > place.identifier.mutable_range.end.0
        {
            place.identifier.mutable_range.end = InstructionId(end);
        }

        // Apply effect override
        if let Some(&eff) = operand_effects.get(&place.identifier.id) {
            place.effect = eff;
        }
    };

    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            apply(place);
        }
        InstructionValue::StoreLocal { value: val, .. }
        | InstructionValue::StoreContext { value: val, .. } => {
            apply(val);
        }
        InstructionValue::Destructure { value: val, .. } => {
            apply(val);
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            apply(left);
            apply(right);
        }
        InstructionValue::UnaryExpression { value: val, .. } => {
            apply(val);
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            apply(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => apply(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            apply(receiver);
            apply(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => apply(p),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            apply(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => apply(p),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        apply(&mut p.place);
                        if let ObjectPropertyKey::Computed(place) = &mut p.key {
                            apply(place);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => apply(p),
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => apply(p),
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
                apply(p);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => apply(place),
                    JsxAttribute::SpreadAttribute { argument } => apply(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    apply(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                apply(child);
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            apply(object);
        }
        InstructionValue::PropertyStore {
            object, value: val, ..
        } => {
            apply(object);
            apply(val);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            apply(object);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            apply(object);
            apply(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            apply(object);
            apply(property);
            apply(val);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            apply(object);
            apply(property);
        }
        InstructionValue::StoreGlobal { value: val, .. } => {
            apply(val);
        }
        InstructionValue::TypeCastExpression { value: val, .. } => {
            apply(val);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            apply(tag);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for s in subexprs {
                apply(s);
            }
        }
        InstructionValue::Await { value: val, .. } => {
            apply(val);
        }
        InstructionValue::GetIterator { collection, .. } => {
            apply(collection);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            apply(iterator);
            apply(collection);
        }
        InstructionValue::NextPropertyOf { value: val, .. } => {
            apply(val);
        }
        InstructionValue::PrefixUpdate { value: val, .. }
        | InstructionValue::PostfixUpdate { value: val, .. } => {
            apply(val);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            apply(decl);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            apply(test);
            apply(consequent);
            apply(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            apply(left);
            apply(right);
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            for operand in &mut lowered_func.func.context {
                apply(operand);
            }
        }
        // No operands
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. }
        | InstructionValue::Debugger { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a minimal Place with the given IdentifierId.
    fn make_place(id: u32) -> Place {
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

    /// Helper: create a minimal HIRFunction with the given blocks.
    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: Some("test".to_string()),
            fn_type: ReactFunctionType::Component,
            params: vec![Argument::Place(make_place(0))],
            returns: make_place(99),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        }
    }

    /// Helper: create a basic block with instructions and a goto terminal.
    fn make_block(
        id: u32,
        instructions: Vec<Instruction>,
        terminal_id: u32,
    ) -> (BlockId, BasicBlock) {
        (
            BlockId(id),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(id),
                instructions,
                terminal: Terminal::Return {
                    value: make_place(99),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(terminal_id),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: vec![],
            },
        )
    }

    #[test]
    fn test_no_mutations_leaves_defaults() {
        // A function with a single instruction that has no effects.
        // The mutable range should get a default start=instr_id, end=instr_id+1.
        let instr = Instruction {
            id: InstructionId(1),
            lvalue: make_place(10),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Number(42.0),
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        };
        let blocks = vec![make_block(0, vec![instr], 2)];
        let mut func = make_hir_function(blocks);
        let _ = infer_mutation_aliasing_ranges(&mut func, false);

        // The lvalue should have its range set to [1, 2)
        let lvalue = &func.body.blocks[0].1.instructions[0].lvalue;
        assert_eq!(lvalue.identifier.mutable_range.start, InstructionId(1));
        assert_eq!(lvalue.identifier.mutable_range.end, InstructionId(2));
        assert_eq!(lvalue.effect, Effect::ConditionallyMutate);
    }

    #[test]
    fn test_mutate_extends_range() {
        // Instruction 1: Create obj (id=10)
        // Instruction 2: Mutate obj (id=10) — should extend its range to id=3
        let place10 = make_place(10);
        let instr1 = Instruction {
            id: InstructionId(1),
            lvalue: place10.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Create {
                into: place10.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            }]),
        };
        let instr2 = Instruction {
            id: InstructionId(2),
            lvalue: make_place(11),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Mutate {
                value: place10.clone(),
                reason: None,
            }]),
        };
        let blocks = vec![make_block(0, vec![instr1, instr2], 3)];
        let mut func = make_hir_function(blocks);
        let _ = infer_mutation_aliasing_ranges(&mut func, false);

        // id=10's mutable_range.end should be at least 3 (instr_id 2 + 1)
        let lvalue = &func.body.blocks[0].1.instructions[0].lvalue;
        assert!(
            lvalue.identifier.mutable_range.end.0 >= 3,
            "Expected end >= 3, got {}",
            lvalue.identifier.mutable_range.end.0
        );
    }

    #[test]
    fn test_alias_propagates_mutation() {
        // Instruction 1: Create obj_a (id=10)
        // Instruction 2: Create obj_b (id=11), Alias from=10 into=11
        // Instruction 3: Mutate obj_b (id=11) — should also extend obj_a's range
        let place10 = make_place(10);
        let place11 = make_place(11);

        let instr1 = Instruction {
            id: InstructionId(1),
            lvalue: place10.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Create {
                into: place10.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            }]),
        };
        let instr2 = Instruction {
            id: InstructionId(2),
            lvalue: place11.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![
                AliasingEffect::Create {
                    into: place11.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::Other,
                },
                AliasingEffect::Alias {
                    from: place10.clone(),
                    into: place11.clone(),
                },
            ]),
        };
        let instr3 = Instruction {
            id: InstructionId(3),
            lvalue: make_place(12),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Mutate {
                value: place11.clone(),
                reason: None,
            }]),
        };

        let blocks = vec![make_block(0, vec![instr1, instr2, instr3], 4)];
        let mut func = make_hir_function(blocks);
        let _ = infer_mutation_aliasing_ranges(&mut func, false);

        // obj_a (id=10) should have its range extended to at least 4 (instr 3 + 1)
        // because mutating obj_b propagates back through the alias edge
        let lvalue_a = &func.body.blocks[0].1.instructions[0].lvalue;
        assert!(
            lvalue_a.identifier.mutable_range.end.0 >= 4,
            "Expected obj_a end >= 4, got {}",
            lvalue_a.identifier.mutable_range.end.0
        );

        // obj_b (id=11) should also have its range extended to at least 4
        let lvalue_b = &func.body.blocks[0].1.instructions[1].lvalue;
        assert!(
            lvalue_b.identifier.mutable_range.end.0 >= 4,
            "Expected obj_b end >= 4, got {}",
            lvalue_b.identifier.mutable_range.end.0
        );
    }

    #[test]
    fn test_capture_does_not_propagate_non_transitive_mutation() {
        // Instruction 1: Create obj_a (id=10)
        // Instruction 2: Create obj_b (id=11), Capture from=10 into=11
        // Instruction 3: Mutate (non-transitive) obj_b (id=11) — should NOT extend obj_a's range
        let place10 = make_place(10);
        let place11 = make_place(11);

        let instr1 = Instruction {
            id: InstructionId(1),
            lvalue: place10.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Create {
                into: place10.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            }]),
        };
        let instr2 = Instruction {
            id: InstructionId(2),
            lvalue: place11.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![
                AliasingEffect::Create {
                    into: place11.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::Other,
                },
                AliasingEffect::Capture {
                    from: place10.clone(),
                    into: place11.clone(),
                },
            ]),
        };
        let instr3 = Instruction {
            id: InstructionId(3),
            lvalue: make_place(12),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            // Non-transitive mutation — uses Mutate not MutateTransitive
            effects: Some(vec![AliasingEffect::Mutate {
                value: place11.clone(),
                reason: None,
            }]),
        };

        let blocks = vec![make_block(0, vec![instr1, instr2, instr3], 4)];
        let mut func = make_hir_function(blocks);
        let _ = infer_mutation_aliasing_ranges(&mut func, false);

        // obj_a (id=10) should NOT have its range extended past its creation
        // because non-transitive mutations don't propagate through captures
        let lvalue_a = &func.body.blocks[0].1.instructions[0].lvalue;
        assert!(
            lvalue_a.identifier.mutable_range.end.0 < 4,
            "Expected obj_a end < 4 (non-transitive mutation should not propagate through capture), got {}",
            lvalue_a.identifier.mutable_range.end.0
        );
    }

    #[test]
    fn test_transitive_mutation_propagates_through_capture() {
        // Instruction 1: Create obj_a (id=10)
        // Instruction 2: Create obj_b (id=11), Capture from=10 into=11
        // Instruction 3: MutateTransitive obj_b (id=11) — SHOULD extend obj_a's range
        let place10 = make_place(10);
        let place11 = make_place(11);

        let instr1 = Instruction {
            id: InstructionId(1),
            lvalue: place10.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::Create {
                into: place10.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            }]),
        };
        let instr2 = Instruction {
            id: InstructionId(2),
            lvalue: place11.clone(),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![
                AliasingEffect::Create {
                    into: place11.clone(),
                    value: ValueKind::Mutable,
                    reason: ValueReason::Other,
                },
                AliasingEffect::Capture {
                    from: place10.clone(),
                    into: place11.clone(),
                },
            ]),
        };
        let instr3 = Instruction {
            id: InstructionId(3),
            lvalue: make_place(12),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: Some(vec![AliasingEffect::MutateTransitive {
                value: place11.clone(),
            }]),
        };

        let blocks = vec![make_block(0, vec![instr1, instr2, instr3], 4)];
        let mut func = make_hir_function(blocks);
        let _ = infer_mutation_aliasing_ranges(&mut func, false);

        // obj_a (id=10) should have its range extended to at least 4
        // because transitive mutations DO propagate through captures
        let lvalue_a = &func.body.blocks[0].1.instructions[0].lvalue;
        assert!(
            lvalue_a.identifier.mutable_range.end.0 >= 4,
            "Expected obj_a end >= 4 (transitive mutation should propagate through capture), got {}",
            lvalue_a.identifier.mutable_range.end.0
        );
    }

    #[test]
    fn test_return_terminal_effect_function_expression() {
        let instr = Instruction {
            id: InstructionId(1),
            lvalue: make_place(10),
            value: InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        };
        let blocks = vec![make_block(0, vec![instr], 2)];
        let mut func = make_hir_function(blocks);

        // When is_function_expression=true, return terminal should have Effect::Read
        let _ = infer_mutation_aliasing_ranges(&mut func, true);
        if let Terminal::Return { value, .. } = &func.body.blocks[0].1.terminal {
            assert_eq!(value.effect, Effect::Read);
        } else {
            panic!("Expected Return terminal");
        }

        // When is_function_expression=false, return terminal should have Effect::Freeze
        let blocks2 = vec![make_block(0, vec![], 1)];
        let mut func2 = make_hir_function(blocks2);
        let _ = infer_mutation_aliasing_ranges(&mut func2, false);
        if let Terminal::Return { value, .. } = &func2.body.blocks[0].1.terminal {
            assert_eq!(value.effect, Effect::Freeze);
        } else {
            panic!("Expected Return terminal");
        }
    }
}
