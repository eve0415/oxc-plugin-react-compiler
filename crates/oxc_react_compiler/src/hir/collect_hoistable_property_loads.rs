//! Collect hoistable property loads via control flow graph analysis.
//!
//! Port of `CollectHoistablePropertyLoads.ts` from upstream React Compiler
//! (babel-plugin-react-compiler v1.0.0).
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Uses control flow graph analysis to determine which `Identifier`s can be
//! assumed to be non-null objects, on a per-block basis. This is critical for
//! determining which dependency paths can be "hoisted" (moved earlier without
//! risk of null access).

use std::collections::{HashMap, HashSet};

use crate::hir::builder::each_terminal_successor;
use crate::hir::propagate_scope_dependencies_hir::ResolvedDep;
use crate::hir::types::*;

// ---------------------------------------------------------------------------
// PropertyPathNode arena
// ---------------------------------------------------------------------------

/// Index into the `PropertyPathRegistry` arena.
type PropertyPathNodeId = usize;

/// A node in the property path trie. Stored in a `Vec` arena and referenced by
/// index (`PropertyPathNodeId`). Root nodes have `parent: None` and `root: Some(id)`.
#[derive(Debug, Clone)]
struct PropertyPathNode {
    /// Non-optional child properties: property string -> node index.
    properties: HashMap<String, PropertyPathNodeId>,
    /// Optional child properties: property string -> node index.
    optional_properties: HashMap<String, PropertyPathNodeId>,
    /// Parent node index, `None` for root nodes.
    parent: Option<PropertyPathNodeId>,
    /// The full dependency path from root to this node.
    full_path: ReactiveScopeDependency,
    /// Whether any segment on the path from root to this node is optional.
    has_optional: bool,
    /// For root nodes only: the `IdentifierId` of the root identifier.
    root: Option<IdentifierId>,
}

// ---------------------------------------------------------------------------
// PropertyPathRegistry
// ---------------------------------------------------------------------------

/// A trie-like registry for deduplicating property load paths (e.g. `a.b.c`).
/// Nodes are arena-allocated in a `Vec` and referenced by index.
#[derive(Debug)]
struct PropertyPathRegistry {
    /// Arena of property path nodes.
    nodes: Vec<PropertyPathNode>,
    /// Maps root `IdentifierId` to the node index of the root node.
    roots: HashMap<IdentifierId, PropertyPathNodeId>,
}

impl PropertyPathRegistry {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            roots: HashMap::new(),
        }
    }

    /// Get or create a root node for the given identifier.
    fn get_or_create_identifier(&mut self, identifier: &Identifier) -> PropertyPathNodeId {
        if let Some(&id) = self.roots.get(&identifier.id) {
            return id;
        }

        let node_id = self.nodes.len();
        self.nodes.push(PropertyPathNode {
            properties: HashMap::new(),
            optional_properties: HashMap::new(),
            parent: None,
            full_path: ReactiveScopeDependency {
                identifier: identifier.clone(),
                path: Vec::new(),
            },
            has_optional: false,
            root: Some(identifier.id),
        });
        self.roots.insert(identifier.id, node_id);
        node_id
    }

    /// Get or create a child property node under the given parent.
    fn get_or_create_property_entry(
        &mut self,
        parent_id: PropertyPathNodeId,
        entry: &DependencyPathEntry,
    ) -> PropertyPathNodeId {
        // Check if child already exists.
        let existing = if entry.optional {
            self.nodes[parent_id]
                .optional_properties
                .get(&entry.property)
                .copied()
        } else {
            self.nodes[parent_id]
                .properties
                .get(&entry.property)
                .copied()
        };

        if let Some(child_id) = existing {
            return child_id;
        }

        // Build the full path for the new child.
        let parent_full_path = self.nodes[parent_id].full_path.clone();
        let parent_has_optional = self.nodes[parent_id].has_optional;

        let mut new_path = parent_full_path.path.clone();
        new_path.push(entry.clone());

        let child_id = self.nodes.len();
        self.nodes.push(PropertyPathNode {
            properties: HashMap::new(),
            optional_properties: HashMap::new(),
            parent: Some(parent_id),
            full_path: ReactiveScopeDependency {
                identifier: parent_full_path.identifier,
                path: new_path,
            },
            has_optional: parent_has_optional || entry.optional,
            root: None,
        });

        if entry.optional {
            self.nodes[parent_id]
                .optional_properties
                .insert(entry.property.clone(), child_id);
        } else {
            self.nodes[parent_id]
                .properties
                .insert(entry.property.clone(), child_id);
        }

        child_id
    }

    /// Get or create the full property path for a `ReactiveScopeDependency`.
    fn get_or_create_property(&mut self, dep: &ReactiveScopeDependency) -> PropertyPathNodeId {
        let mut curr = self.get_or_create_identifier(&dep.identifier);
        for entry in &dep.path {
            curr = self.get_or_create_property_entry(curr, entry);
        }
        curr
    }
}

// ---------------------------------------------------------------------------
// BlockInfo
// ---------------------------------------------------------------------------

/// Per-block information about which property paths are assumed non-null.
#[derive(Debug, Clone)]
pub struct BlockInfo {
    pub block_id: BlockId,
    /// Set of `PropertyPathNodeId`s that are assumed non-null in this block.
    pub assumed_non_null_objects: HashSet<PropertyPathNodeId>,
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct CollectContext<'a> {
    temporaries: &'a HashMap<IdentifierId, ResolvedDep>,
    known_immutable_identifiers: HashSet<IdentifierId>,
    hoistable_from_optionals: &'a HashMap<BlockId, ReactiveScopeDependency>,
    registry: PropertyPathRegistry,
    /// For nested/inner function declarations: context variables that are
    /// immutable. `None` for the outermost function.
    nested_fn_immutable_context: Option<HashSet<IdentifierId>>,
    /// Functions which are assumed to be eventually called.
    assumed_invoked_fns: HashSet<LoweredFunctionPtr>,
    /// Config flags from environment (passed separately since HIRFunction
    /// does not carry an `env` field in the Rust port).
    enable_treat_function_deps_as_conditional: bool,
    enable_preserve_existing_memoization_guarantees: bool,
}

/// A pointer-equality wrapper around `*const LoweredFunction` so we can put
/// lowered function references into sets. In the upstream TypeScript code,
/// `Set<LoweredFunction>` uses reference identity. We emulate this with raw
/// pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LoweredFunctionPtr(*const LoweredFunction);

// SAFETY: We only use these pointers for identity comparison within a single
// call to `collect_hoistable_property_loads`. The referenced `LoweredFunction`
// values are owned by the `HIRFunction` and live for the duration of the call.
unsafe impl Send for LoweredFunctionPtr {}
unsafe impl Sync for LoweredFunctionPtr {}

// ---------------------------------------------------------------------------
// Helper: in_range check
// ---------------------------------------------------------------------------

/// Check if an instruction id falls within a mutable range (start inclusive,
/// end exclusive). Mirrors upstream `inRange`.
fn in_range(id: InstructionId, range: &MutableRange) -> bool {
    id >= range.start && id < range.end
}

// ---------------------------------------------------------------------------
// is_immutable_at_instr
// ---------------------------------------------------------------------------

fn is_immutable_at_instr(
    identifier: &Identifier,
    instr: InstructionId,
    ctx: &CollectContext<'_>,
) -> bool {
    if let Some(ref nested_ctx) = ctx.nested_fn_immutable_context {
        // Comparing instruction ids across inner-outer function bodies is not
        // valid, as they are numbered independently.
        return nested_ctx.contains(&identifier.id);
    }

    // Since this runs *after* buildReactiveScopeTerminals, identifier mutable
    // ranges are not valid with respect to current instruction id numbering.
    // We use attached reactive scope ranges as a proxy for mutable range.
    let mutable_at_instr = identifier.mutable_range.end
        > InstructionId(identifier.mutable_range.start.0 + 1)
        && identifier.scope.is_some()
        && {
            let scope = identifier.scope.as_ref().unwrap();
            in_range(instr, &scope.range)
        };

    !mutable_at_instr || ctx.known_immutable_identifiers.contains(&identifier.id)
}

// ---------------------------------------------------------------------------
// get_maybe_non_null_in_instruction
// ---------------------------------------------------------------------------

fn collect_conditionally_evaluated_identifier_ids(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut definitions: HashMap<IdentifierId, &Instruction> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            definitions.insert(instr.lvalue.identifier.id, instr);
        }
    }

    fn collect_from_place(
        place: &Place,
        definitions: &HashMap<IdentifierId, &Instruction>,
        out: &mut HashSet<IdentifierId>,
        seen: &mut HashSet<IdentifierId>,
    ) {
        if !seen.insert(place.identifier.id) {
            return;
        }
        out.insert(place.identifier.id);
        if let Some(def_instr) = definitions.get(&place.identifier.id).copied() {
            crate::hir::visitors::for_each_instruction_value_operand(&def_instr.value, |inner| {
                collect_from_place(inner, definitions, out, seen);
            });
        }
        seen.remove(&place.identifier.id);
    }

    let mut conditional_ids = HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LogicalExpression { right, .. } => {
                    let mut seen = HashSet::new();
                    collect_from_place(right, &definitions, &mut conditional_ids, &mut seen);
                }
                _ => {}
            }
        }
    }

    conditional_ids
}

fn get_maybe_non_null_in_instruction(
    instr: &Instruction,
    conditionally_evaluated_ids: &HashSet<IdentifierId>,
    ctx: &mut CollectContext<'_>,
) -> Option<PropertyPathNodeId> {
    if conditionally_evaluated_ids.contains(&instr.lvalue.identifier.id) {
        return None;
    }

    let path: Option<ReactiveScopeDependency> = match &instr.value {
        InstructionValue::PropertyLoad {
            object, optional, ..
        } => {
            if *optional {
                return None;
            }
            Some(
                ctx.temporaries
                    .get(&object.identifier.id)
                    .map(|dep| ReactiveScopeDependency {
                        identifier: dep.identifier.clone(),
                        path: dep.path.clone(),
                    })
                    .unwrap_or_else(|| ReactiveScopeDependency {
                        identifier: object.identifier.clone(),
                        path: Vec::new(),
                    }),
            )
        }
        InstructionValue::Destructure { value, .. } => ctx
            .temporaries
            .get(&value.identifier.id)
            .map(|dep| ReactiveScopeDependency {
                identifier: dep.identifier.clone(),
                path: dep.path.clone(),
            }),
        InstructionValue::ComputedLoad {
            object, optional, ..
        } => {
            if *optional {
                return None;
            }
            ctx.temporaries
                .get(&object.identifier.id)
                .map(|dep| ReactiveScopeDependency {
                    identifier: dep.identifier.clone(),
                    path: dep.path.clone(),
                })
        }
        _ => None,
    };

    path.map(|p| ctx.registry.get_or_create_property(&p))
}

// ---------------------------------------------------------------------------
// collect_non_nulls_in_blocks
// ---------------------------------------------------------------------------

fn collect_non_nulls_in_blocks(
    func: &HIRFunction,
    ctx: &mut CollectContext<'_>,
) -> HashMap<BlockId, BlockInfo> {
    let conditionally_evaluated_ids = collect_conditionally_evaluated_identifier_ids(func);

    // Known non-null objects such as functional component props can be safely
    // read from any block.
    let mut known_non_null_identifiers: HashSet<PropertyPathNodeId> = HashSet::new();
    if func.fn_type == ReactFunctionType::Component && !func.params.is_empty() {
        if let Argument::Place(ref place) = func.params[0] {
            let node_id = ctx.registry.get_or_create_identifier(&place.identifier);
            known_non_null_identifiers.insert(node_id);
        }
    }

    let mut nodes: HashMap<BlockId, BlockInfo> = HashMap::new();

    for (_, block) in &func.body.blocks {
        let mut assumed_non_null_objects = known_non_null_identifiers.clone();

        // Check if this block has a hoistable optional chain.
        if let Some(optional_chain) = ctx.hoistable_from_optionals.get(&block.id) {
            let node_id = ctx.registry.get_or_create_property(optional_chain);
            assumed_non_null_objects.insert(node_id);
        }

        for instr in &block.instructions {
            let maybe_non_null =
                get_maybe_non_null_in_instruction(instr, &conditionally_evaluated_ids, ctx);
            if let Some(node_id) = maybe_non_null {
                let full_path_ident = ctx.registry.nodes[node_id].full_path.identifier.clone();
                let immutable = is_immutable_at_instr(&full_path_ident, instr.id, ctx);
                if immutable {
                    assumed_non_null_objects.insert(node_id);
                }
            }

            // Handle FunctionExpression: recurse into assumed-invoked inner functions.
            if let InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value {
                let ptr = LoweredFunctionPtr(lowered_func as *const LoweredFunction);
                if ctx.assumed_invoked_fns.contains(&ptr) {
                    // Build nested immutable context if not already set.
                    let nested_fn_immutable_context =
                        ctx.nested_fn_immutable_context.clone().unwrap_or_else(|| {
                            lowered_func
                                .func
                                .context
                                .iter()
                                .filter(|place| {
                                    is_immutable_at_instr(&place.identifier, instr.id, ctx)
                                })
                                .map(|place| place.identifier.id)
                                .collect()
                        });

                    // Save and swap context for recursive call.
                    let saved_nested = ctx.nested_fn_immutable_context.take();
                    ctx.nested_fn_immutable_context = Some(nested_fn_immutable_context);

                    let inner_map = collect_hoistable_property_loads_impl(&lowered_func.func, ctx);

                    // Restore context.
                    ctx.nested_fn_immutable_context = saved_nested;

                    if let Some(inner_entry) = inner_map.get(&lowered_func.func.body.entry) {
                        for &entry_id in &inner_entry.assumed_non_null_objects {
                            assumed_non_null_objects.insert(entry_id);
                        }
                    }
                }
            }

            // Handle StartMemoize with deps (when enablePreserveExistingMemoizationGuarantees).
            if ctx.enable_preserve_existing_memoization_guarantees {
                if let InstructionValue::StartMemoize {
                    deps: Some(deps), ..
                } = &instr.value
                {
                    for dep in deps {
                        if let ManualMemoRoot::NamedLocal(ref place) = dep.root {
                            if !is_immutable_at_instr(&place.identifier, instr.id, ctx) {
                                continue;
                            }
                            for i in 0..dep.path.len() {
                                let path_entry = &dep.path[i];
                                if path_entry.optional {
                                    break;
                                }
                                let sub_dep = ReactiveScopeDependency {
                                    identifier: place.identifier.clone(),
                                    path: dep.path[..i].to_vec(),
                                };
                                let dep_node = ctx.registry.get_or_create_property(&sub_dep);
                                assumed_non_null_objects.insert(dep_node);
                            }
                        }
                    }
                }
            }
        }

        nodes.insert(
            block.id,
            BlockInfo {
                block_id: block.id,
                assumed_non_null_objects,
            },
        );
    }

    nodes
}

// ---------------------------------------------------------------------------
// Set helpers
// ---------------------------------------------------------------------------

fn set_intersect(sets: &[&HashSet<PropertyPathNodeId>]) -> HashSet<PropertyPathNodeId> {
    if sets.is_empty() {
        return HashSet::new();
    }
    if sets.len() == 1 {
        return sets[0].clone();
    }

    // Start with the smallest set for efficiency.
    let (smallest_idx, _) = sets
        .iter()
        .enumerate()
        .min_by_key(|(_, s)| s.len())
        .unwrap();

    let mut result = sets[smallest_idx].clone();
    for (i, set) in sets.iter().enumerate() {
        if i == smallest_idx {
            continue;
        }
        result.retain(|item| set.contains(item));
    }
    result
}

fn set_union(
    a: &HashSet<PropertyPathNodeId>,
    b: &HashSet<PropertyPathNodeId>,
) -> HashSet<PropertyPathNodeId> {
    let mut result = a.clone();
    for item in b {
        result.insert(*item);
    }
    result
}

fn set_equal(a: &HashSet<PropertyPathNodeId>, b: &HashSet<PropertyPathNodeId>) -> bool {
    a.len() == b.len() && a.iter().all(|item| b.contains(item))
}

// ---------------------------------------------------------------------------
// reduce_maybe_optional_chains
// ---------------------------------------------------------------------------

/// Any two optional chains with different operations `.` vs `?.` but the same
/// set of property string paths de-duplicates.
///
/// Given `<base>?.b`, if unconditional reads from `<base>` are hoistable,
/// replace all `<base>?.PROPERTY` subpaths with `<base>.PROPERTY`.
fn reduce_maybe_optional_chains(
    nodes: &mut HashSet<PropertyPathNodeId>,
    registry: &mut PropertyPathRegistry,
) {
    let optional_chain_nodes: Vec<PropertyPathNodeId> = nodes
        .iter()
        .filter(|&&n| registry.nodes[n].has_optional)
        .copied()
        .collect();

    if optional_chain_nodes.is_empty() {
        return;
    }

    let mut current_optional: HashSet<PropertyPathNodeId> =
        optional_chain_nodes.into_iter().collect();

    let mut changed = true;
    while changed {
        changed = false;

        let to_process: Vec<PropertyPathNodeId> = current_optional.iter().copied().collect();
        for original in to_process {
            let full_path = registry.nodes[original].full_path.clone();
            let identifier = full_path.identifier.clone();
            let orig_path = full_path.path.clone();

            let mut curr_node = registry.get_or_create_identifier(&identifier);
            for entry in &orig_path {
                // If the base is known to be non-null, replace with a non-optional load.
                let next_entry = if entry.optional && nodes.contains(&curr_node) {
                    DependencyPathEntry {
                        property: entry.property.clone(),
                        optional: false,
                    }
                } else {
                    entry.clone()
                };
                curr_node = registry.get_or_create_property_entry(curr_node, &next_entry);
            }

            if curr_node != original {
                changed = true;
                current_optional.remove(&original);
                current_optional.insert(curr_node);
                nodes.remove(&original);
                nodes.insert(curr_node);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// predecessors
// ---------------------------------------------------------------------------

/// Compute predecessor map from block metadata.
fn compute_predecessors_for_propagation(func: &HIRFunction) -> HashMap<BlockId, HashSet<BlockId>> {
    let mut pred_map: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    for (pred_id, block) in &func.body.blocks {
        for succ in each_terminal_successor(&block.terminal) {
            pred_map.entry(succ).or_default().insert(*pred_id);
        }
    }
    pred_map
}

// ---------------------------------------------------------------------------
// propagate_non_null (fixed-point CFG propagation)
// ---------------------------------------------------------------------------

fn propagate_non_null(
    func: &HIRFunction,
    nodes: &mut HashMap<BlockId, BlockInfo>,
    registry: &mut PropertyPathRegistry,
) {
    // Build correct predecessor map using upstream's eachTerminalSuccessor semantics.
    let correct_preds = compute_predecessors_for_propagation(func);

    // Build successor map from correct predecessors.
    let mut block_successors: HashMap<BlockId, HashSet<BlockId>> = HashMap::new();
    let mut _terminal_preds: HashSet<BlockId> = HashSet::new();

    for (block_id, preds) in &correct_preds {
        for pred in preds {
            block_successors.entry(*pred).or_default().insert(*block_id);
        }
    }
    for (_, block) in &func.body.blocks {
        match &block.terminal {
            Terminal::Throw { .. } | Terminal::Return { .. } => {
                _terminal_preds.insert(block.id);
            }
            _ => {}
        }
    }

    // Recursive propagation helper.
    fn recursively_propagate_non_null(
        node_id: BlockId,
        direction: Direction,
        traversal_state: &mut HashMap<BlockId, TraversalStatus>,
        nodes: &mut HashMap<BlockId, BlockInfo>,
        block_successors: &HashMap<BlockId, HashSet<BlockId>>,
        correct_preds: &HashMap<BlockId, HashSet<BlockId>>,
        registry: &mut PropertyPathRegistry,
    ) -> bool {
        if traversal_state.contains_key(&node_id) {
            return false;
        }
        traversal_state.insert(node_id, TraversalStatus::Active);

        let node = nodes.get(&node_id);
        if node.is_none() {
            panic!(
                "[CollectHoistablePropertyLoads] Bad node {}, kind: {:?}",
                node_id, direction
            );
        }

        // Get neighbors based on direction.
        let neighbors: Vec<BlockId> = match direction {
            Direction::Backward => block_successors
                .get(&node_id)
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default(),
            Direction::Forward => {
                // Use the correct predecessors computed with upstream semantics.
                correct_preds
                    .get(&node_id)
                    .map(|s| s.iter().copied().collect())
                    .unwrap_or_default()
            }
        };

        let mut changed = false;
        for pred in &neighbors {
            if !traversal_state.contains_key(pred) {
                let neighbor_changed = recursively_propagate_non_null(
                    *pred,
                    direction,
                    traversal_state,
                    nodes,
                    block_successors,
                    correct_preds,
                    registry,
                );
                changed |= neighbor_changed;
            }
        }

        // Intersect the assumed-non-null sets of all "done" neighbors.
        let done_neighbor_sets: Vec<HashSet<PropertyPathNodeId>> = neighbors
            .iter()
            .filter(|n| traversal_state.get(n) == Some(&TraversalStatus::Done))
            .filter_map(|n| nodes.get(n))
            .map(|info| info.assumed_non_null_objects.clone())
            .collect();

        let done_refs: Vec<&HashSet<PropertyPathNodeId>> = done_neighbor_sets.iter().collect();
        let neighbor_accesses = set_intersect(&done_refs);

        let prev_objects = nodes
            .get(&node_id)
            .unwrap()
            .assumed_non_null_objects
            .clone();
        let mut merged_objects = set_union(&prev_objects, &neighbor_accesses);
        reduce_maybe_optional_chains(&mut merged_objects, registry);

        let objects_changed = !set_equal(&prev_objects, &merged_objects);
        nodes.get_mut(&node_id).unwrap().assumed_non_null_objects = merged_objects;

        traversal_state.insert(node_id, TraversalStatus::Done);
        changed |= objects_changed;
        changed
    }

    let reversed_blocks: Vec<BlockId> = func.body.blocks.iter().rev().map(|(id, _)| *id).collect();
    let forward_blocks: Vec<BlockId> = func.body.blocks.iter().map(|(id, _)| *id).collect();

    let mut changed;
    let mut iteration = 0;
    loop {
        assert!(
            iteration < 100,
            "[CollectHoistablePropertyLoads] fixed point iteration did not terminate after 100 loops"
        );
        iteration += 1;

        changed = false;
        let mut traversal_state: HashMap<BlockId, TraversalStatus> = HashMap::new();

        for &block_id in &forward_blocks {
            let forward_changed = recursively_propagate_non_null(
                block_id,
                Direction::Forward,
                &mut traversal_state,
                nodes,
                &block_successors,
                &correct_preds,
                registry,
            );
            changed |= forward_changed;
        }

        traversal_state.clear();

        for &block_id in &reversed_blocks {
            let backward_changed = recursively_propagate_non_null(
                block_id,
                Direction::Backward,
                &mut traversal_state,
                nodes,
                &block_successors,
                &correct_preds,
                registry,
            );
            changed |= backward_changed;
        }

        if !changed {
            break;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraversalStatus {
    Active,
    Done,
}

// ---------------------------------------------------------------------------
// get_assumed_invoked_functions
// ---------------------------------------------------------------------------

/// Identifies functions that are assumed to be eventually invoked (e.g. direct
/// calls, hooks arguments, JSX attributes). This is used to inline non-null
/// information from inner functions into their parent scope.
fn get_assumed_invoked_functions(func: &HIRFunction) -> HashSet<LoweredFunctionPtr> {
    let mut temporaries: HashMap<IdentifierId, (LoweredFunctionPtr, HashSet<LoweredFunctionPtr>)> =
        HashMap::new();

    get_assumed_invoked_functions_impl(func, &mut temporaries)
}

fn get_assumed_invoked_functions_impl(
    func: &HIRFunction,
    temporaries: &mut HashMap<IdentifierId, (LoweredFunctionPtr, HashSet<LoweredFunctionPtr>)>,
) -> HashSet<LoweredFunctionPtr> {
    let mut hoistable_functions: HashSet<LoweredFunctionPtr> = HashSet::new();

    // Step 1: Conservatively collect identifier -> function expression mappings.
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    let ptr = LoweredFunctionPtr(lowered_func as *const LoweredFunction);
                    temporaries.insert(instr.lvalue.identifier.id, (ptr, HashSet::new()));
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if let Some(entry) = temporaries.get(&value.identifier.id).cloned() {
                        temporaries.insert(lvalue.place.identifier.id, entry);
                    }
                }
                InstructionValue::LoadLocal { place, .. } => {
                    if let Some(entry) = temporaries.get(&place.identifier.id).cloned() {
                        temporaries.insert(instr.lvalue.identifier.id, entry);
                    }
                }
                _ => {}
            }
        }
    }

    // Step 2: Forward pass to analyze assumed function calls.
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::CallExpression { callee, args, .. } => {
                    let maybe_hook = is_hook_function_type(&callee.identifier.type_);

                    if let Some(entry) = temporaries.get(&callee.identifier.id) {
                        // Direct calls.
                        hoistable_functions.insert(entry.0);
                    } else if maybe_hook {
                        // Assume arguments to all hooks are safe to invoke.
                        for arg in args {
                            if let Argument::Place(place) = arg {
                                if let Some(entry) = temporaries.get(&place.identifier.id) {
                                    hoistable_functions.insert(entry.0);
                                }
                            }
                        }
                    }
                }
                InstructionValue::JsxExpression {
                    props, children, ..
                } => {
                    // Assume JSX attributes and children are safe to invoke.
                    for attr in props {
                        match attr {
                            JsxAttribute::SpreadAttribute { .. } => continue,
                            JsxAttribute::Attribute { place, .. } => {
                                if let Some(entry) = temporaries.get(&place.identifier.id) {
                                    hoistable_functions.insert(entry.0);
                                }
                            }
                        }
                    }
                    if let Some(children_vec) = children {
                        for child in children_vec {
                            if let Some(entry) = temporaries.get(&child.identifier.id) {
                                hoistable_functions.insert(entry.0);
                            }
                        }
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. } => {
                    // Recursively traverse inner function expressions.
                    let lambdas_called =
                        get_assumed_invoked_functions_impl(&lowered_func.func, temporaries);
                    if let Some(entry) = temporaries.get_mut(&instr.lvalue.identifier.id) {
                        for called in &lambdas_called {
                            entry.1.insert(*called);
                        }
                    }
                }
                _ => {}
            }
        }

        // Check return terminals for directly returned functions.
        if let Terminal::Return { ref value, .. } = block.terminal {
            if let Some(entry) = temporaries.get(&value.identifier.id) {
                hoistable_functions.insert(entry.0);
            }
        }
    }

    // Step 3: Propagate transitive calls.
    let entries: Vec<(LoweredFunctionPtr, HashSet<LoweredFunctionPtr>)> =
        temporaries.values().cloned().collect();
    for (fn_ptr, may_invoke) in &entries {
        if hoistable_functions.contains(fn_ptr) {
            for called in may_invoke {
                hoistable_functions.insert(*called);
            }
        }
    }

    hoistable_functions
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
                | "BuiltInUseLayoutEffectHookId"
                | "BuiltInUseInsertionEffectHookId"
                | "BuiltInUseTransitionHookId"
                | "BuiltInUseImperativeHandleHookId"
                | "BuiltInUseActionStateHookId"
                | "BuiltInDefaultMutatingHookId"
                | "BuiltInDefaultNonmutatingHookId"
                | "SharedRuntimeUseFragmentHook"
                | "SharedRuntimeUseNoAliasHook"
                | "SharedRuntimeUseFreezeHook"
        ),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// collect_hoistable_property_loads_impl
// ---------------------------------------------------------------------------

fn collect_hoistable_property_loads_impl(
    func: &HIRFunction,
    ctx: &mut CollectContext<'_>,
) -> HashMap<BlockId, BlockInfo> {
    let mut nodes = collect_non_nulls_in_blocks(func, ctx);
    propagate_non_null(func, &mut nodes, &mut ctx.registry);
    nodes
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Main entry point: collect hoistable property loads for the given function.
///
/// # Arguments
/// * `func` - The HIR function to analyze.
/// * `temporaries` - Side map of identifier id -> base object path (from
///   `PropagateScopeDependenciesHIR`).
/// * `hoistable_from_optionals` - Side map of optional block -> base optional
///   path for which non-optional loads are safe.
///
/// # Returns
/// A map from `BlockId` to `BlockInfo` containing the assumed non-null objects.
pub fn collect_hoistable_property_loads(
    func: &HIRFunction,
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
    hoistable_from_optionals: &HashMap<BlockId, ReactiveScopeDependency>,
) -> HashMap<BlockId, BlockInfo> {
    // Collect known immutable identifiers (function params for Components/Hooks).
    let mut known_immutable_identifiers = HashSet::new();
    if func.fn_type == ReactFunctionType::Component || func.fn_type == ReactFunctionType::Hook {
        for p in &func.params {
            if let Argument::Place(place) = p {
                known_immutable_identifiers.insert(place.identifier.id);
            }
        }
    }

    // Determine assumed invoked functions.
    // When enableTreatFunctionDepsAsConditional is true, we use an empty set.
    // Default: false (so we compute assumed invoked functions).
    let assumed_invoked_fns = get_assumed_invoked_functions(func);

    let mut ctx = CollectContext {
        temporaries,
        known_immutable_identifiers,
        hoistable_from_optionals,
        registry: PropertyPathRegistry::new(),
        nested_fn_immutable_context: None,
        assumed_invoked_fns,
        enable_treat_function_deps_as_conditional: false,
        enable_preserve_existing_memoization_guarantees: false,
    };

    collect_hoistable_property_loads_impl(func, &mut ctx)
}

/// Variant that accepts environment config flags explicitly.
pub fn collect_hoistable_property_loads_with_config(
    func: &HIRFunction,
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
    hoistable_from_optionals: &HashMap<BlockId, ReactiveScopeDependency>,
    enable_treat_function_deps_as_conditional: bool,
    enable_preserve_existing_memoization_guarantees: bool,
) -> HashMap<BlockId, BlockInfo> {
    let mut known_immutable_identifiers = HashSet::new();
    if func.fn_type == ReactFunctionType::Component || func.fn_type == ReactFunctionType::Hook {
        for p in &func.params {
            if let Argument::Place(place) = p {
                known_immutable_identifiers.insert(place.identifier.id);
            }
        }
    }

    let assumed_invoked_fns = if enable_treat_function_deps_as_conditional {
        HashSet::new()
    } else {
        get_assumed_invoked_functions(func)
    };

    let mut ctx = CollectContext {
        temporaries,
        known_immutable_identifiers,
        hoistable_from_optionals,
        registry: PropertyPathRegistry::new(),
        nested_fn_immutable_context: None,
        assumed_invoked_fns,
        enable_treat_function_deps_as_conditional,
        enable_preserve_existing_memoization_guarantees,
    };

    collect_hoistable_property_loads_impl(func, &mut ctx)
}

/// Collect hoistable property loads and return resolved full dependency paths
/// for the function entry block.
///
/// This mirrors upstream `collectHoistablePropertyLoadsInInnerFn`, which is
/// used when inferring minimal dependencies for lowered function expressions.
pub fn collect_hoistable_property_loads_in_inner_fn(
    fn_instr: &Instruction,
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
    hoistable_from_optionals: &HashMap<BlockId, ReactiveScopeDependency>,
) -> Vec<ReactiveScopeDependency> {
    let func = match &fn_instr.value {
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => &lowered_func.func,
        _ => return Vec::new(),
    };

    let enable_treat_function_deps_as_conditional =
        func.env.config().enable_treat_function_deps_as_conditional;
    let assumed_invoked_fns = if enable_treat_function_deps_as_conditional {
        HashSet::new()
    } else {
        get_assumed_invoked_functions(func)
    };

    let mut initial_ctx = CollectContext {
        temporaries,
        known_immutable_identifiers: HashSet::new(),
        hoistable_from_optionals,
        registry: PropertyPathRegistry::new(),
        nested_fn_immutable_context: None,
        assumed_invoked_fns,
        enable_treat_function_deps_as_conditional,
        enable_preserve_existing_memoization_guarantees: false,
    };

    let nested_fn_immutable_context: HashSet<IdentifierId> = func
        .context
        .iter()
        .filter(|place| is_immutable_at_instr(&place.identifier, fn_instr.id, &initial_ctx))
        .map(|place| place.identifier.id)
        .collect();
    initial_ctx.nested_fn_immutable_context = Some(nested_fn_immutable_context);

    let raw_block_infos = collect_hoistable_property_loads_impl(func, &mut initial_ctx);
    raw_block_infos
        .get(&func.body.entry)
        .map(|info| {
            info.assumed_non_null_objects
                .iter()
                .map(|&node_id| initial_ctx.registry.nodes[node_id].full_path.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Maps `BlockId`-keyed results to `ScopeId`-keyed results by scanning for
/// `Terminal::Scope` blocks.
pub fn key_by_scope_id(
    func: &HIRFunction,
    source: &HashMap<BlockId, BlockInfo>,
) -> HashMap<ScopeId, BlockInfo> {
    let mut keyed_by_scope_id: HashMap<ScopeId, BlockInfo> = HashMap::new();

    for (_, block) in &func.body.blocks {
        if let Terminal::Scope {
            scope,
            block: scope_block,
            ..
        } = &block.terminal
        {
            if let Some(info) = source.get(scope_block) {
                keyed_by_scope_id.insert(scope.id, info.clone());
            }
        }
    }

    keyed_by_scope_id
}

/// Collect hoistable property loads and return them keyed by scope ID with
/// resolved full paths. This is the main entry point for integration with
/// `propagate_scope_dependencies_hir`.
///
/// Combines `collect_hoistable_property_loads` and `key_by_scope_id`, resolving
/// arena-based `PropertyPathNodeId` indices to full `ReactiveScopeDependency`
/// paths before returning.
pub fn collect_hoistable_property_loads_for_scopes(
    func: &HIRFunction,
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
    hoistable_from_optionals: &HashMap<BlockId, ReactiveScopeDependency>,
) -> HashMap<ScopeId, Vec<ReactiveScopeDependency>> {
    let mut known_immutable_identifiers = HashSet::new();
    if func.fn_type == ReactFunctionType::Component || func.fn_type == ReactFunctionType::Hook {
        for p in &func.params {
            if let Argument::Place(place) = p {
                known_immutable_identifiers.insert(place.identifier.id);
            }
        }
    }

    let enable_treat_function_deps_as_conditional =
        func.env.config().enable_treat_function_deps_as_conditional;
    let enable_preserve_existing_memoization_guarantees = func
        .env
        .config()
        .enable_preserve_existing_memoization_guarantees;
    let assumed_invoked_fns = if enable_treat_function_deps_as_conditional {
        HashSet::new()
    } else {
        get_assumed_invoked_functions(func)
    };

    let mut ctx = CollectContext {
        temporaries,
        known_immutable_identifiers,
        hoistable_from_optionals,
        registry: PropertyPathRegistry::new(),
        nested_fn_immutable_context: None,
        assumed_invoked_fns,
        enable_treat_function_deps_as_conditional,
        enable_preserve_existing_memoization_guarantees,
    };

    let raw_block_infos = collect_hoistable_property_loads_impl(func, &mut ctx);

    if std::env::var("DEBUG_BLOCK_NONNULL").is_ok() {
        for (block_id, info) in &raw_block_infos {
            if !info.assumed_non_null_objects.is_empty() {
                let resolved: Vec<_> = info
                    .assumed_non_null_objects
                    .iter()
                    .map(|&node_id| &ctx.registry.nodes[node_id].full_path)
                    .collect();
                eprintln!(
                    "[BLOCK_NONNULL] block={} count={} paths={:?}",
                    block_id,
                    info.assumed_non_null_objects.len(),
                    resolved
                        .iter()
                        .map(|p| format!("id={}:{:?}", p.identifier.id.0, p.path))
                        .collect::<Vec<_>>()
                );
            }
        }
        // Show block structure
        for (block_id, block) in &func.body.blocks {
            let term_desc = match &block.terminal {
                Terminal::Scope {
                    scope,
                    block: inner,
                    fallthrough,
                    ..
                } => format!(
                    "Scope(id={}, block={}, fall={})",
                    scope.id.0, inner, fallthrough
                ),
                Terminal::PrunedScope {
                    scope,
                    block: inner,
                    fallthrough,
                    ..
                } => format!(
                    "PrunedScope(id={}, block={}, fall={})",
                    scope.id.0, inner, fallthrough
                ),
                Terminal::Goto { block: target, .. } => format!("Goto({})", target),
                Terminal::If {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => format!(
                    "If(cons={}, alt={}, fall={:?})",
                    consequent, alternate, fallthrough
                ),
                Terminal::Return { .. } => "Return".to_string(),
                _ => format!("{:?}", std::mem::discriminant(&block.terminal)),
            };
            eprintln!(
                "[BLOCK_CFG] block={} preds={:?} term={}",
                block_id, block.preds, term_desc
            );
        }
    }

    // Key by scope ID and resolve arena indices to full dependency paths
    let mut result = HashMap::new();
    for (_, block) in &func.body.blocks {
        if let Terminal::Scope {
            scope,
            block: scope_block,
            ..
        } = &block.terminal
        {
            if let Some(info) = raw_block_infos.get(scope_block) {
                let resolved: Vec<ReactiveScopeDependency> = info
                    .assumed_non_null_objects
                    .iter()
                    .map(|&node_id| ctx.registry.nodes[node_id].full_path.clone())
                    .collect();
                result.insert(scope.id, resolved);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_identifier(id: u32, name: Option<&str>) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: name.map(|n| IdentifierName::Named(n.to_string())),
            mutable_range: MutableRange::default(),
            scope: None,
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32, name: Option<&str>) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Unknown,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_basic_func(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            loc: SourceLocation::Generated,
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![Argument::Place(make_place(0, Some("props")))],
            returns: make_place(99, None),
            context: Vec::new(),
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: Vec::new(),
            aliasing_effects: None,
        }
    }

    #[test]
    fn test_property_path_registry_basics() {
        let mut registry = PropertyPathRegistry::new();
        let ident = make_identifier(1, Some("x"));

        let root = registry.get_or_create_identifier(&ident);
        assert_eq!(root, 0);
        assert!(registry.nodes[root].root.is_some());
        assert_eq!(registry.nodes[root].full_path.path.len(), 0);

        // Create a.b path.
        let entry_a = DependencyPathEntry {
            property: "a".to_string(),
            optional: false,
        };
        let node_a = registry.get_or_create_property_entry(root, &entry_a);
        assert_eq!(registry.nodes[node_a].full_path.path.len(), 1);
        assert_eq!(registry.nodes[node_a].full_path.path[0].property, "a");
        assert!(!registry.nodes[node_a].has_optional);

        // Create a?.b path (optional).
        let entry_b_opt = DependencyPathEntry {
            property: "b".to_string(),
            optional: true,
        };
        let node_b = registry.get_or_create_property_entry(node_a, &entry_b_opt);
        assert_eq!(registry.nodes[node_b].full_path.path.len(), 2);
        assert!(registry.nodes[node_b].has_optional);

        // Re-fetching should return the same node.
        let node_a2 = registry.get_or_create_property_entry(root, &entry_a);
        assert_eq!(node_a, node_a2);
    }

    #[test]
    fn test_set_helpers() {
        let a: HashSet<usize> = [1, 2, 3].into_iter().collect();
        let b: HashSet<usize> = [2, 3, 4].into_iter().collect();

        let intersection = set_intersect(&[&a, &b]);
        assert_eq!(intersection, [2, 3].into_iter().collect());

        let union = set_union(&a, &b);
        assert_eq!(union, [1, 2, 3, 4].into_iter().collect());

        assert!(set_equal(&a, &a));
        assert!(!set_equal(&a, &b));
    }

    #[test]
    fn test_collect_simple_function() {
        // Build a simple function with one block that does a PropertyLoad.
        let blocks = vec![(
            BlockId(0),
            BasicBlock {
                kind: BlockKind::Block,
                id: BlockId(0),
                instructions: vec![
                    // $1 = LoadLocal 'props'
                    Instruction {
                        id: InstructionId(1),
                        lvalue: make_place(1, None),
                        value: InstructionValue::LoadLocal {
                            place: make_place(0, Some("props")),
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                    // $2 = PropertyLoad $1.foo
                    Instruction {
                        id: InstructionId(2),
                        lvalue: make_place(2, None),
                        value: InstructionValue::PropertyLoad {
                            object: make_place(1, None),
                            property: PropertyLiteral::String("foo".to_string()),
                            optional: false,
                            loc: SourceLocation::Generated,
                        },
                        loc: SourceLocation::Generated,
                        effects: None,
                    },
                ],
                terminal: Terminal::Return {
                    value: make_place(2, None),
                    return_variant: ReturnVariant::Explicit,
                    id: InstructionId(3),
                    loc: SourceLocation::Generated,
                },
                preds: HashSet::new(),
                phis: Vec::new(),
            },
        )];

        let func = make_basic_func(blocks);

        // Set up temporaries: $1 -> {identifier: props, path: []}
        let mut temporaries = HashMap::new();
        temporaries.insert(
            IdentifierId(1),
            ResolvedDep {
                identifier: make_identifier(0, Some("props")),
                path: Vec::new(),
            },
        );

        let hoistable_from_optionals = HashMap::new();
        let result =
            collect_hoistable_property_loads(&func, &temporaries, &hoistable_from_optionals);

        // Block 0 should have at least the props identifier as non-null
        // (since it's a Component and props is the first param).
        let block_info = result.get(&BlockId(0)).unwrap();
        assert!(!block_info.assumed_non_null_objects.is_empty());
    }

    #[test]
    fn test_in_range() {
        let range = MutableRange {
            start: InstructionId(5),
            end: InstructionId(10),
        };
        assert!(in_range(InstructionId(5), &range));
        assert!(in_range(InstructionId(7), &range));
        assert!(in_range(InstructionId(9), &range));
        assert!(!in_range(InstructionId(10), &range));
        assert!(!in_range(InstructionId(4), &range));
    }
}
