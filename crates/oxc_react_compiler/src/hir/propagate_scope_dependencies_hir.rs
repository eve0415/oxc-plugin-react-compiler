//! Scope-terminal-aware dependency propagation for reactive scopes.
//!
//! Port of `PropagateScopeDependenciesHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass populates `scope.dependencies`, `scope.declarations`, and
//! `scope.reassignments` for each reactive scope. It uses scope terminals
//! (Terminal::Scope / Terminal::PrunedScope) to track scope boundaries,
//! replacing the range-based approach in `propagate_scope_dependencies.rs`.
//!
//! Key improvements over the old pass:
//! - Uses scope terminals for precise scope boundary detection
//! - Integrates with ReactiveScopeDependencyTreeHIR for dependency minimization
//! - Properly handles inner functions (FunctionExpression / ObjectMethod)
//! - Tracks temporaries used outside their declaring scope

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::hir::collect_optional_chain_deps::{ProcessedOptionalNode, ProcessedOptionalNodeKind};
use crate::hir::types::*;

// ---------------------------------------------------------------------------
// ScopeBlockTraversal: detect scope begin/end from terminals
// ---------------------------------------------------------------------------

/// Information about a scope boundary at a given block.
#[derive(Debug, Clone)]
enum ScopeBlockInfo {
    Begin { scope: ReactiveScope, pruned: bool },
    End { scope: ReactiveScope, pruned: bool },
}

/// Build a map from BlockId to scope boundary info by scanning all terminals.
fn build_scope_block_infos(func: &HIRFunction) -> HashMap<BlockId, ScopeBlockInfo> {
    let mut infos: HashMap<BlockId, ScopeBlockInfo> = HashMap::new();

    for (_block_id, block) in &func.body.blocks {
        match &block.terminal {
            Terminal::Scope {
                block: inner_block,
                fallthrough,
                scope,
                ..
            } => {
                infos.insert(
                    *inner_block,
                    ScopeBlockInfo::Begin {
                        scope: scope.clone(),
                        pruned: false,
                    },
                );
                infos.insert(
                    *fallthrough,
                    ScopeBlockInfo::End {
                        scope: scope.clone(),
                        pruned: false,
                    },
                );
            }
            Terminal::PrunedScope {
                block: inner_block,
                fallthrough,
                scope,
                ..
            } => {
                infos.insert(
                    *inner_block,
                    ScopeBlockInfo::Begin {
                        scope: scope.clone(),
                        pruned: true,
                    },
                );
                infos.insert(
                    *fallthrough,
                    ScopeBlockInfo::End {
                        scope: scope.clone(),
                        pruned: true,
                    },
                );
            }
            _ => {}
        }
    }

    infos
}

// ---------------------------------------------------------------------------
// Temporaries sidemap
// ---------------------------------------------------------------------------

/// A resolved dependency: base identifier + property path.
#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub identifier: Identifier,
    pub path: Vec<DependencyPathEntry>,
}

/// Find temporaries that are used outside their declaring scope.
///
/// Port of `findTemporariesUsedOutsideDeclaringScope` from upstream.
fn find_temporaries_used_outside_declaring_scope(
    func: &HIRFunction,
    scope_infos: &HashMap<BlockId, ScopeBlockInfo>,
) -> HashSet<DeclarationId> {
    let mut declarations: HashMap<DeclarationId, ScopeId> = HashMap::new();
    let mut pruned_scopes: HashSet<ScopeId> = HashSet::new();
    let mut active_scopes: Vec<ScopeId> = Vec::new();
    let mut used_outside: HashSet<DeclarationId> = HashSet::new();

    for (block_id, block) in &func.body.blocks {
        // Process scope boundaries for this block
        if let Some(info) = scope_infos.get(block_id) {
            match info {
                ScopeBlockInfo::Begin { scope, pruned } => {
                    active_scopes.push(scope.id);
                    if *pruned {
                        pruned_scopes.insert(scope.id);
                    }
                }
                ScopeBlockInfo::End { scope, .. } => {
                    if let Some(pos) = active_scopes.iter().rposition(|s| *s == scope.id) {
                        active_scopes.remove(pos);
                    }
                }
            }
        }

        let current_scope = active_scopes.last().copied();

        for instr in &block.instructions {
            // Handle operand places: check if used outside declaring scope
            crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                let decl_scope = declarations.get(&place.identifier.declaration_id);
                if let Some(&decl_scope_id) = decl_scope {
                    let is_active = active_scopes.contains(&decl_scope_id);
                    let is_pruned = pruned_scopes.contains(&decl_scope_id);
                    if !is_active && !is_pruned {
                        used_outside.insert(place.identifier.declaration_id);
                    }
                }
            });

            // Handle lvalue: record declarations
            if let Some(scope_id) = current_scope
                && !pruned_scopes.contains(&scope_id)
            {
                match &instr.value {
                    InstructionValue::LoadLocal { .. }
                    | InstructionValue::LoadContext { .. }
                    | InstructionValue::PropertyLoad { .. } => {
                        declarations.insert(instr.lvalue.identifier.declaration_id, scope_id);
                    }
                    _ => {}
                }
            }
        }

        // Terminal operands
        crate::hir::visitors::for_each_terminal_operand(&block.terminal, |place| {
            let decl_scope = declarations.get(&place.identifier.declaration_id);
            if let Some(&decl_scope_id) = decl_scope {
                let is_active = active_scopes.contains(&decl_scope_id);
                let is_pruned = pruned_scopes.contains(&decl_scope_id);
                if !is_active && !is_pruned {
                    used_outside.insert(place.identifier.declaration_id);
                }
            }
        });
    }

    used_outside
}

/// Build temporaries sidemap: resolve LoadLocal/PropertyLoad chains to their
/// base identifier + property path.
///
/// Port of `collectTemporariesSidemap` from upstream.
fn collect_temporaries_sidemap(
    func: &HIRFunction,
    used_outside: &HashSet<DeclarationId>,
) -> HashMap<IdentifierId, ResolvedDep> {
    let mut temporaries: HashMap<IdentifierId, ResolvedDep> = HashMap::new();
    let jsx_consumed_decls = collect_jsx_consumed_declarations(func);
    collect_temporaries_impl(
        func,
        used_outside,
        &mut temporaries,
        &jsx_consumed_decls,
        None,
    );
    temporaries
}

/// Recursive implementation for collecting temporaries.
///
/// `inner_fn_context`: For inner functions, the instruction ID of the enclosing
/// FunctionExpression in the outermost function. `None` for the outermost
/// function. Upstream equivalent: `innerFnContext: {instrId: InstructionId} | null`.
fn collect_temporaries_impl(
    func: &HIRFunction,
    used_outside: &HashSet<DeclarationId>,
    temporaries: &mut HashMap<IdentifierId, ResolvedDep>,
    jsx_consumed_decls: &HashSet<DeclarationId>,
    inner_fn_context: Option<InstructionId>,
) {
    let is_inner_fn = inner_fn_context.is_some();
    let inner_fn_has_try_catch = is_inner_fn && function_contains_try_catch(func);
    let mut empty_array_temps: HashSet<IdentifierId> = HashSet::new();
    let mut nullish_alias_temps: HashSet<IdentifierId> = HashSet::new();
    let mut nullish_literal_temps: HashSet<IdentifierId> = HashSet::new();
    for (_block_id, block) in &func.body.blocks {
        for instr in &block.instructions {
            let is_used_outside = used_outside.contains(&instr.lvalue.identifier.declaration_id);
            // For inner functions, use the outer function's instruction ID for
            // mutability checks (upstream: `innerFnContext.instrId`).
            let effective_instr_id = inner_fn_context.unwrap_or(instr.id);

            match &instr.value {
                InstructionValue::Primitive {
                    value: PrimitiveValue::Null | PrimitiveValue::Undefined,
                    ..
                } => {
                    nullish_literal_temps.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::ArrayExpression { elements, .. } if elements.is_empty() => {
                    empty_array_temps.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::PropertyLoad {
                    object,
                    property,
                    optional,
                    ..
                } => {
                    let base_dep = temporaries.get(&object.identifier.id);
                    let preserve_optional_chain_temp = *optional
                        || base_dep.is_some_and(|dep| dep.path.iter().any(|entry| entry.optional));
                    if is_used_outside && !preserve_optional_chain_temp {
                        continue;
                    }
                    // Only track if we can resolve the object through the sidemap
                    // (or if we're in the outermost function, or if the object is a context variable)
                    if !is_inner_fn
                        || temporaries.contains_key(&object.identifier.id)
                        || func
                            .context
                            .iter()
                            .any(|ctx| ctx.identifier.id == object.identifier.id)
                    {
                        // OXC may lower some optional-member chains without the
                        // exact optional-terminal shape consumed by
                        // `collect_optional_chain_sidemap`. Preserve the load
                        // token here so deps can still retain source `?.` markers.
                        let resolved = get_property(object, property, *optional, temporaries);
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] prop lvalue id={} decl={} name={:?} <- base id={} decl={} name={:?} path={:?}",
                                instr.lvalue.identifier.id.0,
                                instr.lvalue.identifier.declaration_id.0,
                                instr.lvalue.identifier.name,
                                resolved.identifier.id.0,
                                resolved.identifier.declaration_id.0,
                                resolved.identifier.name,
                                resolved.path
                            );
                        }
                        temporaries.insert(instr.lvalue.identifier.id, resolved);
                    }
                }
                InstructionValue::LogicalExpression {
                    operator: LogicalOperator::NullishCoalescing,
                    left,
                    right,
                    ..
                } if !is_inner_fn && !is_used_outside => {
                    // Narrow alias canonicalization: only track `x = y ?? []` where
                    // `y` already resolves to a single non-optional property path.
                    if !empty_array_temps.contains(&right.identifier.id) {
                        continue;
                    }
                    if let Some(resolved_left) = temporaries
                        .get(&left.identifier.id)
                        .cloned()
                        .filter(|dep| dep.path.len() == 1 && !dep.path[0].optional)
                    {
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] nullish-empty-array lvalue id={} decl={} <- base id={} decl={} name={:?} path={:?}",
                                instr.lvalue.identifier.id.0,
                                instr.lvalue.identifier.declaration_id.0,
                                resolved_left.identifier.id.0,
                                resolved_left.identifier.declaration_id.0,
                                resolved_left.identifier.name,
                                resolved_left.path
                            );
                        }
                        temporaries.insert(instr.lvalue.identifier.id, resolved_left);
                        nullish_alias_temps.insert(instr.lvalue.identifier.id);
                    }
                }
                InstructionValue::Ternary {
                    test,
                    consequent,
                    alternate,
                    ..
                } if !is_inner_fn
                    && !is_used_outside
                    && jsx_consumed_decls.contains(&instr.lvalue.identifier.declaration_id)
                    && instr.lvalue.identifier.name.is_none() =>
                {
                    // Flattened conditional temporaries (e.g. props.a ? props.b : props.c)
                    // should behave like upstream nested expressions. If all operands
                    // resolve to the same root, map this temporary to that root.
                    let test_dep =
                        temporaries
                            .get(&test.identifier.id)
                            .cloned()
                            .unwrap_or(ResolvedDep {
                                identifier: test.identifier.clone(),
                                path: Vec::new(),
                            });
                    let consequent_dep = temporaries
                        .get(&consequent.identifier.id)
                        .cloned()
                        .unwrap_or(ResolvedDep {
                            identifier: consequent.identifier.clone(),
                            path: Vec::new(),
                        });
                    let alternate_dep = temporaries
                        .get(&alternate.identifier.id)
                        .cloned()
                        .unwrap_or(ResolvedDep {
                            identifier: alternate.identifier.clone(),
                            path: Vec::new(),
                        });
                    let root_decl = test_dep.identifier.declaration_id;
                    let same_root = consequent_dep.identifier.declaration_id == root_decl
                        && alternate_dep.identifier.declaration_id == root_decl;
                    let has_property_path = !test_dep.path.is_empty()
                        || !consequent_dep.path.is_empty()
                        || !alternate_dep.path.is_empty();
                    if same_root && has_property_path {
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] ternary-root lvalue id={} decl={} <- base id={} decl={} name={:?}",
                                instr.lvalue.identifier.id.0,
                                instr.lvalue.identifier.declaration_id.0,
                                test_dep.identifier.id.0,
                                root_decl.0,
                                test_dep.identifier.name
                            );
                        }
                        temporaries.insert(
                            instr.lvalue.identifier.id,
                            ResolvedDep {
                                identifier: test_dep.identifier,
                                path: Vec::new(),
                            },
                        );
                    }
                }
                InstructionValue::BinaryExpression {
                    operator,
                    left,
                    right,
                    ..
                } if !is_inner_fn && !is_used_outside => {
                    if !matches!(
                        operator,
                        BinaryOperator::Eq
                            | BinaryOperator::NotEq
                            | BinaryOperator::StrictEq
                            | BinaryOperator::StrictNotEq
                    ) {
                        continue;
                    }

                    let compared_place = if nullish_literal_temps.contains(&left.identifier.id) {
                        Some(right)
                    } else if nullish_literal_temps.contains(&right.identifier.id) {
                        Some(left)
                    } else {
                        None
                    };

                    let Some(compared_place) = compared_place else {
                        continue;
                    };

                    let resolved = temporaries
                        .get(&compared_place.identifier.id)
                        .cloned()
                        .unwrap_or(ResolvedDep {
                            identifier: compared_place.identifier.clone(),
                            path: Vec::new(),
                        });

                    if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                        eprintln!(
                            "[TEMP_SIDEMAP] nullish-binary lvalue id={} decl={} <- base id={} decl={} name={:?} path={:?}",
                            instr.lvalue.identifier.id.0,
                            instr.lvalue.identifier.declaration_id.0,
                            resolved.identifier.id.0,
                            resolved.identifier.declaration_id.0,
                            resolved.identifier.name,
                            resolved.path
                        );
                    }
                    temporaries.insert(instr.lvalue.identifier.id, resolved);
                }
                InstructionValue::LoadLocal { place, .. }
                    if instr.lvalue.identifier.name.is_none()
                        && place.identifier.name.is_some()
                        && !is_used_outside =>
                {
                    if !is_inner_fn
                        || func
                            .context
                            .iter()
                            .any(|ctx| ctx.identifier.id == place.identifier.id)
                    {
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] local lvalue id={} decl={} <- base id={} decl={} name={:?}",
                                instr.lvalue.identifier.id.0,
                                instr.lvalue.identifier.declaration_id.0,
                                place.identifier.id.0,
                                place.identifier.declaration_id.0,
                                place.identifier.name
                            );
                        }
                        let resolved =
                            temporaries
                                .get(&place.identifier.id)
                                .cloned()
                                .unwrap_or(ResolvedDep {
                                    identifier: place.identifier.clone(),
                                    path: Vec::new(),
                                });
                        temporaries.insert(instr.lvalue.identifier.id, resolved);
                    }
                }
                InstructionValue::LoadContext { place, .. }
                    if instr.lvalue.identifier.name.is_none()
                        && place.identifier.name.is_some()
                        && !is_used_outside =>
                {
                    let is_past_mutable_range = place
                        .identifier
                        .scope
                        .as_ref()
                        .is_some_and(|scope| effective_instr_id >= scope.range.end);
                    let track_inner_load_context = is_inner_fn && !inner_fn_has_try_catch;
                    if (track_inner_load_context || is_past_mutable_range)
                        && (!is_inner_fn
                            || func
                                .context
                                .iter()
                                .any(|ctx| ctx.identifier.id == place.identifier.id))
                    {
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] context lvalue id={} decl={} <- base id={} decl={} name={:?}",
                                instr.lvalue.identifier.id.0,
                                instr.lvalue.identifier.declaration_id.0,
                                place.identifier.id.0,
                                place.identifier.declaration_id.0,
                                place.identifier.name
                            );
                        }
                        let resolved =
                            temporaries
                                .get(&place.identifier.id)
                                .cloned()
                                .unwrap_or(ResolvedDep {
                                    identifier: place.identifier.clone(),
                                    path: Vec::new(),
                                });
                        temporaries.insert(instr.lvalue.identifier.id, resolved);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                    if !is_inner_fn
                        && lvalue.kind == InstructionKind::Const
                        && !is_used_outside
                        && !used_outside.contains(&lvalue.place.identifier.declaration_id)
                        && nullish_alias_temps.contains(&value.identifier.id) =>
                {
                    if !matches!(
                        &lvalue.place.identifier.type_,
                        Type::Object { shape_id: Some(s) } if s == "BuiltInArray"
                    ) {
                        continue;
                    }
                    if let Some(resolved_value) = temporaries.get(&value.identifier.id).cloned() {
                        if std::env::var("DEBUG_TEMP_SIDEMAP").is_ok() {
                            eprintln!(
                                "[TEMP_SIDEMAP] store-const-nullish-array alias id={} decl={} name={:?} <- base id={} decl={} name={:?} path={:?}",
                                lvalue.place.identifier.id.0,
                                lvalue.place.identifier.declaration_id.0,
                                lvalue.place.identifier.name,
                                resolved_value.identifier.id.0,
                                resolved_value.identifier.declaration_id.0,
                                resolved_value.identifier.name,
                                resolved_value.path
                            );
                        }
                        temporaries.insert(lvalue.place.identifier.id, resolved_value);
                    }
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    // Upstream: `innerFnContext ?? {instrId}` - use the outermost
                    // function's instruction ID for all nested levels.
                    let ctx = inner_fn_context.unwrap_or(instr.id);
                    collect_temporaries_impl(
                        &lowered_func.func,
                        used_outside,
                        temporaries,
                        jsx_consumed_decls,
                        Some(ctx),
                    );
                }
                _ => {}
            }
        }
    }
}

fn collect_jsx_consumed_declarations(func: &HIRFunction) -> HashSet<DeclarationId> {
    fn walk_function(func: &HIRFunction, out: &mut HashSet<DeclarationId>) {
        for (_block_id, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::JsxExpression {
                        tag,
                        props,
                        children,
                        ..
                    } => {
                        if let JsxTag::Component(place) = tag {
                            out.insert(place.identifier.declaration_id);
                        }
                        for prop in props {
                            match prop {
                                JsxAttribute::Attribute { place, .. }
                                | JsxAttribute::SpreadAttribute { argument: place } => {
                                    out.insert(place.identifier.declaration_id);
                                }
                            }
                        }
                        if let Some(children) = children {
                            for child in children {
                                out.insert(child.identifier.declaration_id);
                            }
                        }
                    }
                    InstructionValue::JsxFragment { children, .. } => {
                        for child in children {
                            out.insert(child.identifier.declaration_id);
                        }
                    }
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        walk_function(&lowered_func.func, out);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut out = HashSet::new();
    walk_function(func, &mut out);
    out
}

fn function_contains_try_catch(func: &HIRFunction) -> bool {
    for (_, block) in &func.body.blocks {
        if matches!(block.terminal, Terminal::Try { .. }) {
            return true;
        }
        for instr in &block.instructions {
            let has_catch_lvalue = match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    lvalue.kind == InstructionKind::Catch
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    lvalue.kind == InstructionKind::Catch
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    lvalue.kind == InstructionKind::Catch
                }
                _ => false,
            };
            if has_catch_lvalue {
                return true;
            }
        }
    }
    false
}

/// Resolve a property load through the temporaries sidemap.
fn get_property(
    object: &Place,
    property: &PropertyLiteral,
    optional: bool,
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
) -> ResolvedDep {
    let prop_str = match property {
        PropertyLiteral::String(s) => s.clone(),
        PropertyLiteral::Number(n) => n.to_string(),
    };

    let resolved = temporaries.get(&object.identifier.id);
    match resolved {
        Some(base) => {
            let mut path = base.path.clone();
            path.push(DependencyPathEntry {
                property: prop_str,
                optional,
            });
            ResolvedDep {
                identifier: base.identifier.clone(),
                path,
            }
        }
        None => ResolvedDep {
            identifier: object.identifier.clone(),
            path: vec![DependencyPathEntry {
                property: prop_str,
                optional,
            }],
        },
    }
}

// ---------------------------------------------------------------------------
// Dependency collection context
// ---------------------------------------------------------------------------

/// Where a declaration was first seen.
#[derive(Debug, Clone)]
struct DeclInfo {
    /// Instruction ID where it was declared.
    id: InstructionId,
    /// Declaration this info belongs to. Used to guard against identifier-id
    /// collisions across nested lowered functions.
    declaration_id: DeclarationId,
    /// Snapshot of the scope stack at time of declaration.
    /// Used to determine which scopes a value should be declared as an output of.
    scope_stack_snapshot: Vec<ReactiveScope>,
}

/// Context for collecting scope dependencies.
struct DependencyCollectionContext {
    /// Maps declaration_id -> where it was first declared.
    declarations: HashMap<DeclarationId, DeclInfo>,
    /// Maps identifier (by id) -> latest reassignment info.
    reassignments: HashMap<IdentifierId, DeclInfo>,

    /// Stack of active scopes.
    scope_stack: Vec<ReactiveScope>,
    /// Stack of dependency lists (one per active scope).
    dep_stack: Vec<Vec<ReactiveScopeDependency>>,
    /// Final deps for each scope.
    scope_deps: HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    /// Final declarations for each scope.
    scope_decls: HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    /// Final reassignments for each scope.
    scope_reassignments: HashMap<ScopeId, Vec<Identifier>>,

    /// Temporaries sidemap.
    temporaries: HashMap<IdentifierId, ResolvedDep>,
    /// Instructions/terminals processed by collectOptionalChainSidemap — skip these.
    processed_instrs_in_optional: HashSet<ProcessedOptionalNode>,
    /// Active lowered-function identity stack for optional deferred-node lookups.
    fn_key_stack: Vec<usize>,

    /// Whether we're inside an inner function.
    in_inner_fn: bool,

    /// Non-local binding names keyed by declaration.
    /// Populated from `LoadGlobal` so call-site checks can recover names
    /// when temporary identifiers are unnamed.
    non_local_binding_names: HashMap<IdentifierId, String>,
    /// Mirrors `enableTreatRefLikeIdentifiersAsRefs` from env config.
    enable_treat_ref_like_identifiers_as_refs: bool,
    /// Declarations produced by `mergeRefs(...)` calls.
    merge_refs_results: HashSet<IdentifierId>,
    /// Component props parameter declaration id (if any).
    component_props_param_decl: Option<DeclarationId>,
}

impl DependencyCollectionContext {
    fn new(
        temporaries: HashMap<IdentifierId, ResolvedDep>,
        processed_instrs_in_optional: HashSet<ProcessedOptionalNode>,
        enable_treat_ref_like_identifiers_as_refs: bool,
        component_props_param_decl: Option<DeclarationId>,
    ) -> Self {
        Self {
            declarations: HashMap::new(),
            reassignments: HashMap::new(),
            scope_stack: Vec::new(),
            dep_stack: Vec::new(),
            scope_deps: HashMap::new(),
            scope_decls: HashMap::new(),
            scope_reassignments: HashMap::new(),
            temporaries,
            processed_instrs_in_optional,
            fn_key_stack: Vec::new(),
            in_inner_fn: false,
            non_local_binding_names: HashMap::new(),
            enable_treat_ref_like_identifiers_as_refs,
            merge_refs_results: HashSet::new(),
            component_props_param_decl,
        }
    }

    fn enter_function(&mut self, func: &HIRFunction) {
        self.fn_key_stack.push(func as *const HIRFunction as usize);
    }

    fn exit_function(&mut self) {
        self.fn_key_stack.pop();
    }

    fn current_function_key(&self) -> Option<usize> {
        self.fn_key_stack.last().copied()
    }

    fn enter_scope(&mut self, scope: ReactiveScope) {
        self.dep_stack.push(Vec::new());
        self.scope_stack.push(scope);
    }

    fn exit_scope(&mut self, _scope: &ReactiveScope, pruned: bool) {
        let scope_deps = self.dep_stack.pop().unwrap_or_default();
        let exiting_scope = self.scope_stack.pop();

        // Propagate deps upward: child scope deps become parent scope deps
        // if they're valid in the parent context
        for dep in &scope_deps {
            if self.check_valid_dependency(dep)
                && let Some(parent_deps) = self.dep_stack.last_mut()
            {
                parent_deps.push(dep.clone());
            }
        }

        if !pruned && let Some(scope) = &exiting_scope {
            self.scope_deps.insert(scope.id, scope_deps);
        }
    }

    fn declare(&mut self, identifier: &Identifier, id: InstructionId) {
        if self.in_inner_fn {
            return;
        }
        let info = DeclInfo {
            id,
            declaration_id: identifier.declaration_id,
            scope_stack_snapshot: self.scope_stack.clone(),
        };
        self.declarations
            .entry(identifier.declaration_id)
            .or_insert_with(|| info.clone());
        self.reassignments.insert(identifier.id, info);
    }

    fn has_declared(&self, identifier: &Identifier) -> bool {
        self.declarations.contains_key(&identifier.declaration_id)
    }

    fn lookup_decl_info(&self, dep: &ReactiveScopeDependency) -> Option<&DeclInfo> {
        let reassigned = self
            .reassignments
            .get(&dep.identifier.id)
            .filter(|d| d.declaration_id == dep.identifier.declaration_id);
        if reassigned.is_some() {
            return reassigned;
        }
        if self.in_inner_fn {
            // When traversing inner functions, avoid declaration-id fallback:
            // declaration IDs are only unique per lowered function and can
            // collide with outer declarations.
            return None;
        }
        self.declarations.get(&dep.identifier.declaration_id)
    }

    /// Check if a dependency is valid for the current scope.
    fn check_valid_dependency(&self, dep: &ReactiveScopeDependency) -> bool {
        // ref value is not a valid dep
        if is_ref_value_type(&dep.identifier) {
            if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
                eprintln!(
                    "[SCOPE_DEP] reject reason=ref_value id={} decl={}",
                    dep.identifier.id.0, dep.identifier.declaration_id.0
                );
            }
            return false;
        }
        // object methods are not deps because they will be codegen'd back in to
        // the object literal.
        if is_object_method_type(&dep.identifier) {
            if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
                eprintln!(
                    "[SCOPE_DEP] reject reason=object_method id={} decl={}",
                    dep.identifier.id.0, dep.identifier.declaration_id.0
                );
            }
            return false;
        }

        let current_scope = match self.scope_stack.last() {
            Some(s) => s,
            None => {
                if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
                    eprintln!(
                        "[SCOPE_DEP] reject reason=no_current_scope id={} decl={}",
                        dep.identifier.id.0, dep.identifier.declaration_id.0
                    );
                }
                return false;
            }
        };

        // Look up where this was declared
        let decl = self.lookup_decl_info(dep);

        match decl {
            Some(d) => {
                // Must be declared before the current scope starts
                let ok = d.id < current_scope.range.start;
                if std::env::var("DEBUG_SCOPE_DEPS").is_ok() && !ok {
                    eprintln!(
                        "[SCOPE_DEP] reject reason=decl_not_before_scope dep_id={} dep_decl={} decl_instr={} scope_start={}",
                        dep.identifier.id.0,
                        dep.identifier.declaration_id.0,
                        d.id.0,
                        current_scope.range.start.0
                    );
                }
                ok
            }
            None => {
                if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
                    eprintln!(
                        "[SCOPE_DEP] reject reason=decl_not_found id={} decl={}",
                        dep.identifier.id.0, dep.identifier.declaration_id.0
                    );
                }
                false
            }
        }
    }

    /// Resolve a place to its dependency (using temporaries if available).
    fn resolve_operand(&self, place: &Place) -> ReactiveScopeDependency {
        if let Some(resolved) = self.temporaries.get(&place.identifier.id) {
            ReactiveScopeDependency {
                identifier: resolved.identifier.clone(),
                path: resolved.path.clone(),
            }
        } else {
            ReactiveScopeDependency {
                identifier: place.identifier.clone(),
                path: Vec::new(),
            }
        }
    }

    /// Best-effort name recovery for a resolved dependency.
    fn dependency_name<'a>(&'a self, dep: &'a ReactiveScopeDependency) -> Option<&'a str> {
        dep.identifier
            .name
            .as_ref()
            .map(IdentifierName::value)
            .or_else(|| {
                self.non_local_binding_names
                    .get(&dep.identifier.id)
                    .map(|s| s.as_str())
            })
    }

    /// Visit an operand place, potentially adding it as a dependency.
    fn visit_operand(&mut self, place: &Place) {
        let dep = self.resolve_operand(place);
        self.visit_dependency(dep);
    }

    /// Visit an operand that is conditionally evaluated (e.g. optional-call args).
    ///
    /// Keep only the maximal prefix that is safe to evaluate independently from
    /// the conditional call. For optional-member paths like `a?.b.c`, this
    /// yields `a?.b`. For paths without any optional segment, keep the original
    /// path (it is already safe to evaluate).
    fn visit_conditional_operand(&mut self, place: &Place) {
        let mut dep = self.resolve_operand(place);
        if dep.path.is_empty() {
            self.visit_dependency(dep);
            return;
        }
        if let Some(first_optional_idx) = dep.path.iter().position(|entry| entry.optional) {
            let mut truncate_at = dep.path.len();
            for (idx, entry) in dep.path.iter().enumerate().skip(first_optional_idx + 1) {
                if !entry.optional {
                    truncate_at = idx;
                    break;
                }
            }
            dep.path.truncate(truncate_at);
        }
        self.visit_dependency(dep);
    }

    /// Visit an argument to an optional `CallExpression`.
    ///
    /// Upstream optional call chains conservatively collapse non-optional
    /// argument paths to the root object (e.g. `props.a` -> `props`) because
    /// argument evaluation is gated by the optional callee.
    fn visit_optional_call_argument(&mut self, place: &Place) {
        let mut dep = self.resolve_operand(place);
        if dep.path.is_empty() {
            self.visit_dependency(dep);
            return;
        }
        if let Some(first_optional_idx) = dep.path.iter().position(|entry| entry.optional) {
            let mut truncate_at = dep.path.len();
            for (idx, entry) in dep.path.iter().enumerate().skip(first_optional_idx + 1) {
                if !entry.optional {
                    truncate_at = idx;
                    break;
                }
            }
            dep.path.truncate(truncate_at);
        } else {
            dep.path.clear();
        }
        self.visit_dependency(dep);
    }

    /// Visit a callee reference used in an optional call.
    ///
    /// The terminal property segment represents the callable itself and can be
    /// conditionally skipped; track the receiver chain instead.
    fn visit_optional_callee_operand(&mut self, place: &Place) {
        let mut dep = self.resolve_operand(place);
        if !dep.path.is_empty() {
            dep.path.pop();
        }
        if dep.path.is_empty() {
            self.visit_dependency(dep);
            return;
        }
        if let Some(first_optional_idx) = dep.path.iter().position(|entry| entry.optional) {
            let mut truncate_at = dep.path.len();
            for (idx, entry) in dep.path.iter().enumerate().skip(first_optional_idx + 1) {
                if !entry.optional {
                    truncate_at = idx;
                    break;
                }
            }
            dep.path.truncate(truncate_at);
        }
        self.visit_dependency(dep);
    }

    /// Visit a property load, resolving through temporaries.
    fn visit_property(&mut self, object: &Place, property: &PropertyLiteral, optional: bool) {
        let resolved = get_property(object, property, optional, &self.temporaries);
        let dep = ReactiveScopeDependency {
            identifier: resolved.identifier,
            path: resolved.path,
        };
        self.visit_dependency(dep);
    }

    /// Visit a resolved dependency.
    fn visit_dependency(&mut self, mut dep: ReactiveScopeDependency) {
        if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
            eprintln!(
                "[SCOPE_DEP] candidate id={} decl={} name={:?} path={:?}",
                dep.identifier.id.0, dep.identifier.declaration_id.0, dep.identifier.name, dep.path
            );
        }
        // Populate scope.declarations for scopes that computed this value.
        // This mirrors upstream's `originalDeclaration.scope.each(scope => { ... })`
        // which iterates through the scope stack snapshot at the time of declaration
        // and adds the identifier as a declaration to any scope that:
        //   1. Is no longer active (has been exited)
        //   2. Doesn't already have this identifier as a declaration
        // Upstream uses the original declaration map here (not reassignment lookup):
        // outputs are keyed to where a value was first declared, even if later
        // stores/reassignments happen outside that scope.
        let original_decl = self
            .declarations
            .get(&dep.identifier.declaration_id)
            .cloned();
        if let Some(decl) = original_decl
            && !decl.scope_stack_snapshot.is_empty()
        {
            // The innermost (most recently entered) scope at declaration time.
            // This is `originalDeclaration.scope.value!` in upstream.
            let innermost_scope = decl.scope_stack_snapshot.last().unwrap().clone();

            // For each scope that was active when this value was declared,
            // check if it's still active. If not, the value escapes that scope
            // and should be registered as a scope declaration (output).
            for scope in &decl.scope_stack_snapshot {
                let is_active = self.scope_stack.iter().any(|s| s.id == scope.id);
                if !is_active {
                    let scope_decls = self.scope_decls.entry(scope.id).or_default();
                    // Match upstream: dedup by declarationId across all values,
                    // not by IdentifierId key. Upstream checks:
                    //   !Iterable_some(scope.declarations.values(),
                    //     decl => decl.identifier.declarationId === dep.identifier.declarationId)
                    let already_declared = scope_decls
                        .values()
                        .any(|d| d.identifier.declaration_id == dep.identifier.declaration_id);
                    if !already_declared {
                        scope_decls.insert(
                            dep.identifier.id,
                            ScopeDeclaration {
                                identifier: dep.identifier.clone(),
                                scope: innermost_scope.clone(),
                            },
                        );
                    }
                }
            }
        }

        // ref.current access is not a valid dep — truncate to just the ref.
        // When the ref-like-name option is enabled, apply the same rule to
        // named identifiers (ref/*Ref) even if type inference left them as Poly.
        let truncate_ref_current = dep.path.first().is_some_and(|p| p.property == "current")
            && (is_use_ref_type(&dep.identifier)
                || (self.enable_treat_ref_like_identifiers_as_refs
                    && is_ref_like_identifier(&dep.identifier)));
        if truncate_ref_current {
            dep = ReactiveScopeDependency {
                identifier: dep.identifier.clone(),
                path: Vec::new(),
            };
        }

        if self.check_valid_dependency(&dep) {
            if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
                let scope_id = self.scope_stack.last().map(|s| s.id.0);
                eprintln!("[SCOPE_DEP] accepted scope={:?}", scope_id);
            }
            if let Some(deps) = self.dep_stack.last_mut() {
                deps.push(dep);
            }
        } else if std::env::var("DEBUG_SCOPE_DEPS").is_ok() {
            eprintln!("[SCOPE_DEP] rejected");
        }
    }

    /// Record a reassignment of a place in its scope.
    fn visit_reassignment(&mut self, place: &Place) {
        if self.in_inner_fn {
            return;
        }
        let current_scope = match self.scope_stack.last() {
            Some(s) => s.clone(),
            None => return,
        };

        let dep = ReactiveScopeDependency {
            identifier: place.identifier.clone(),
            path: Vec::new(),
        };

        if self.check_valid_dependency(&dep) {
            let reassignments = self
                .scope_reassignments
                .entry(current_scope.id)
                .or_default();
            if !reassignments
                .iter()
                .any(|r| r.declaration_id == place.identifier.declaration_id)
            {
                reassignments.push(place.identifier.clone());
            }
        }
    }

    /// Check if an instruction's dependency is deferred (will be resolved at use-site).
    /// Skips instructions already processed by optional chain collection, plus
    /// temporaries that will be resolved at their use site.
    fn is_deferred_dependency_instr(&self, instr: &Instruction) -> bool {
        let deferred_by_optional = self.current_function_key().is_some_and(|function_key| {
            self.processed_instrs_in_optional
                .contains(&ProcessedOptionalNode {
                    function_key,
                    id: instr.id,
                    kind: ProcessedOptionalNodeKind::Instruction,
                })
        });
        deferred_by_optional || self.temporaries.contains_key(&instr.lvalue.identifier.id)
    }

    /// Check if a terminal's dependency is deferred (processed by optional chain collection).
    fn is_deferred_dependency_terminal(&self, terminal: &Terminal) -> bool {
        self.current_function_key().is_some_and(|function_key| {
            self.processed_instrs_in_optional
                .contains(&ProcessedOptionalNode {
                    function_key,
                    id: terminal.id(),
                    kind: ProcessedOptionalNodeKind::Terminal,
                })
        })
    }
}

// ---------------------------------------------------------------------------
// Main pass
// ---------------------------------------------------------------------------

/// Propagate scope dependencies using scope terminals.
///
/// This is the new HIR-level version that uses Terminal::Scope / PrunedScope
/// for scope boundary detection, replacing the range-based approach.
pub fn propagate_scope_dependencies_hir(func: &mut HIRFunction) {
    let scope_infos = build_scope_block_infos(func);

    // If no scope terminals, nothing to do
    if scope_infos.is_empty() {
        return;
    }

    let used_outside = find_temporaries_used_outside_declaring_scope(func, &scope_infos);
    let mut temporaries = collect_temporaries_sidemap(func, &used_outside);

    // Collect optional chain sidemap (handles a?.b?.c patterns)
    // This produces additional temporaries and a set of instructions to skip.
    let optional_sidemap =
        crate::hir::collect_optional_chain_deps::collect_optional_chain_sidemap(func);

    if std::env::var("DEBUG_OPTIONAL_SIDEMAP").is_ok() {
        eprintln!(
            "[OPTIONAL_SIDEMAP] temps={} processed={} hoistable={}",
            optional_sidemap.temporaries_read_in_optional.len(),
            optional_sidemap.processed_instrs_in_optional.len(),
            optional_sidemap.hoistable_objects.len()
        );
        for (id, dep) in &optional_sidemap.temporaries_read_in_optional {
            let path = dep
                .path
                .iter()
                .map(|p| format!("{}{}", p.property, if p.optional { "?" } else { "" }))
                .collect::<Vec<_>>()
                .join(".");
            eprintln!(
                "[OPTIONAL_SIDEMAP] temp id={} ident={} decl={} path={}",
                id.0, dep.identifier.id.0, dep.identifier.declaration_id.0, path
            );
        }
        for (block_id, dep) in &optional_sidemap.hoistable_objects {
            let path = dep
                .path
                .iter()
                .map(|p| format!("{}{}", p.property, if p.optional { "?" } else { "" }))
                .collect::<Vec<_>>()
                .join(".");
            eprintln!(
                "[OPTIONAL_SIDEMAP] hoistable block={} ident={} decl={} path={}",
                block_id.0, dep.identifier.id.0, dep.identifier.declaration_id.0, path
            );
        }
    }

    // Collect hoistable property loads BEFORE merging optional chain temporaries
    // (upstream passes original temporaries, not merged ones)
    let scoped_hoistable_loads =
        crate::hir::collect_hoistable_property_loads::collect_hoistable_property_loads_for_scopes(
            func,
            &temporaries,
            &optional_sidemap.hoistable_objects,
        );

    // NOW merge temporaries with optional chain temporaries
    for (id, dep) in &optional_sidemap.temporaries_read_in_optional {
        temporaries.insert(
            *id,
            ResolvedDep {
                identifier: dep.identifier.clone(),
                path: dep.path.clone(),
            },
        );
    }

    let processed_instrs = optional_sidemap.processed_instrs_in_optional;

    let mut ctx = DependencyCollectionContext::new(
        temporaries,
        processed_instrs,
        func.env.config().enable_treat_ref_like_identifiers_as_refs,
        if matches!(func.fn_type, ReactFunctionType::Component) {
            func.params.first().map(|param| match param {
                Argument::Place(place) | Argument::Spread(place) => place.identifier.declaration_id,
            })
        } else {
            None
        },
    );

    // Register function parameters
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        ctx.declare(&place.identifier, InstructionId(0));
    }

    // Main traversal
    collect_deps_from_function(func, &scope_infos, &mut ctx);

    // Minimize and apply dependencies using ReactiveScopeDependencyTreeHIR
    minimize_and_apply(
        func,
        &ctx.scope_deps,
        &ctx.scope_decls,
        &ctx.scope_reassignments,
        &scoped_hoistable_loads,
    );
}

/// Infer minimal dependencies for a lowered function expression.
///
/// This mirrors upstream `inferMinimalDependencies` in
/// `InferEffectDependencies.ts` by reusing the same dependency collection and
/// minimization machinery as the main scope propagation pass.
pub(crate) fn infer_minimal_dependencies_for_inner_fn(
    fn_expr_instr: &Instruction,
) -> Vec<ReactiveScopeDependency> {
    let debug_effect_deps = std::env::var("DEBUG_INFER_EFFECT_DEPS").is_ok();
    let lowered = match &fn_expr_instr.value {
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => &lowered_func.func,
        _ => return Vec::new(),
    };
    if debug_effect_deps {
        eprintln!(
            "[INFER_EFFECT_DEPS] inner-fn lowered blocks: count={}",
            lowered.body.blocks.len()
        );
        for (bid, block) in &lowered.body.blocks {
            eprintln!(
                "[INFER_EFFECT_DEPS] inner-fn bb{} instrs={} terminal={}",
                bid.0,
                block.instructions.len(),
                match &block.terminal {
                    Terminal::Return { .. } => "return",
                    Terminal::Throw { .. } => "throw",
                    Terminal::Goto { .. } => "goto",
                    Terminal::Branch { .. } => "branch",
                    Terminal::Switch { .. } => "switch",
                    Terminal::Optional { .. } => "optional",
                    Terminal::Scope { .. } => "scope",
                    Terminal::Try { .. } => "try",
                    Terminal::PrunedScope { .. } => "pruned_scope",
                    Terminal::Label { .. } => "label",
                    Terminal::Unsupported { .. } => "unsupported",
                    _ => "other",
                }
            );
            for instr in &block.instructions {
                eprintln!(
                    "[INFER_EFFECT_DEPS] inner-fn bb{} instr#{} lvalue={} decl={} value={:?}",
                    bid.0,
                    instr.id.0,
                    instr.lvalue.identifier.id.0,
                    instr.lvalue.identifier.declaration_id.0,
                    instr.value
                );
            }
        }
    }

    let used_outside = HashSet::new();
    let mut temporaries = collect_temporaries_sidemap(lowered, &used_outside);
    let optional_sidemap =
        crate::hir::collect_optional_chain_deps::collect_optional_chain_sidemap(lowered);
    if debug_effect_deps {
        eprintln!(
            "[INFER_EFFECT_DEPS] inner-fn optional sidemap: temps={} processed={} hoistable={}",
            optional_sidemap.temporaries_read_in_optional.len(),
            optional_sidemap.processed_instrs_in_optional.len(),
            optional_sidemap.hoistable_objects.len()
        );
        for (id, dep) in &optional_sidemap.temporaries_read_in_optional {
            eprintln!(
                "[INFER_EFFECT_DEPS] inner-fn optional temp id={} -> ident={} decl={} path={}",
                id.0,
                dep.identifier.id.0,
                dep.identifier.declaration_id.0,
                dep.path
                    .iter()
                    .map(|p| format!("{}{}", p.property, if p.optional { "?" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(".")
            );
        }
    }
    let hoistable_to_entry =
        crate::hir::collect_hoistable_property_loads::collect_hoistable_property_loads_in_inner_fn(
            fn_expr_instr,
            &temporaries,
            &optional_sidemap.hoistable_objects,
        );
    if debug_effect_deps {
        eprintln!(
            "[INFER_EFFECT_DEPS] inner-fn hoistable to entry count={}",
            hoistable_to_entry.len()
        );
        for dep in &hoistable_to_entry {
            eprintln!(
                "[INFER_EFFECT_DEPS] inner-fn hoistable ident={} decl={} path={}",
                dep.identifier.id.0,
                dep.identifier.declaration_id.0,
                dep.path
                    .iter()
                    .map(|p| format!("{}{}", p.property, if p.optional { "?" } else { "" }))
                    .collect::<Vec<_>>()
                    .join(".")
            );
        }
    }

    for (id, dep) in &optional_sidemap.temporaries_read_in_optional {
        temporaries.insert(
            *id,
            ResolvedDep {
                identifier: dep.identifier.clone(),
                path: dep.path.clone(),
            },
        );
    }

    let mut ctx = DependencyCollectionContext::new(
        temporaries,
        optional_sidemap.processed_instrs_in_optional,
        lowered
            .env
            .config()
            .enable_treat_ref_like_identifiers_as_refs,
        None,
    );
    for context_dep in &lowered.context {
        ctx.declare(&context_dep.identifier, InstructionId(0));
    }

    let placeholder_scope = ReactiveScope {
        id: ScopeId(0),
        range: MutableRange {
            start: fn_expr_instr.id,
            end: InstructionId(fn_expr_instr.id.0.saturating_add(1)),
        },
        dependencies: Vec::new(),
        declarations: IndexMap::new(),
        reassignments: Vec::new(),
        merged_id: None,
        early_return_value: None,
    };
    ctx.enter_scope(placeholder_scope.clone());
    let empty_scope_infos: HashMap<BlockId, ScopeBlockInfo> = HashMap::new();
    collect_deps_from_function(lowered, &empty_scope_infos, &mut ctx);
    ctx.exit_scope(&placeholder_scope, false);

    let unfiltered = ctx
        .scope_deps
        .get(&placeholder_scope.id)
        .cloned()
        .unwrap_or_default();
    let fn_context_ids: HashSet<IdentifierId> = lowered
        .context
        .iter()
        .map(|dep| dep.identifier.id)
        .collect();
    let filtered: Vec<ReactiveScopeDependency> = unfiltered
        .into_iter()
        .filter(|dep| fn_context_ids.contains(&dep.identifier.id))
        .collect();
    if debug_effect_deps {
        eprintln!(
            "[INFER_EFFECT_DEPS] inner-fn placeholder deps: total={} filtered={} context_decls={:?}",
            ctx.scope_deps
                .get(&placeholder_scope.id)
                .map(|d| d.len())
                .unwrap_or(0),
            filtered.len(),
            fn_context_ids.iter().map(|d| d.0).collect::<Vec<_>>()
        );
        if let Some(all_deps) = ctx.scope_deps.get(&placeholder_scope.id) {
            for dep in all_deps {
                eprintln!(
                    "[INFER_EFFECT_DEPS] inner-fn raw dep ident={} decl={} path={}",
                    dep.identifier.id.0,
                    dep.identifier.declaration_id.0,
                    dep.path
                        .iter()
                        .map(|p| p.property.clone())
                        .collect::<Vec<_>>()
                        .join(".")
                );
            }
        }
        for dep in &filtered {
            eprintln!(
                "[INFER_EFFECT_DEPS] inner-fn dep ident={} decl={} path={}",
                dep.identifier.id.0,
                dep.identifier.declaration_id.0,
                dep.path
                    .iter()
                    .map(|p| p.property.clone())
                    .collect::<Vec<_>>()
                    .join(".")
            );
        }
    }
    let mut tree = crate::hir::derive_minimal_dependencies::ReactiveScopeDependencyTreeHIR::new(
        hoistable_to_entry.clone(),
    );
    for dep in &filtered {
        tree.add_dependency(dep);
    }
    let mut minimal = tree.derive_minimal_dependencies();

    minimal.sort_by(sort_reactive_scope_dependency_for_inner_fn);
    minimal
}

fn sort_reactive_scope_dependency_for_inner_fn(
    a: &ReactiveScopeDependency,
    b: &ReactiveScopeDependency,
) -> std::cmp::Ordering {
    a.identifier
        .declaration_id
        .cmp(&b.identifier.declaration_id)
        .then_with(|| a.path.len().cmp(&b.path.len()))
        .then_with(|| {
            for (ae, be) in a.path.iter().zip(b.path.iter()) {
                let c = ae.property.cmp(&be.property);
                if c != std::cmp::Ordering::Equal {
                    return c;
                }
                let c = ae.optional.cmp(&be.optional);
                if c != std::cmp::Ordering::Equal {
                    return c;
                }
            }
            std::cmp::Ordering::Equal
        })
}

/// Walk the HIR function and collect dependencies.
fn collect_deps_from_function(
    func: &HIRFunction,
    scope_infos: &HashMap<BlockId, ScopeBlockInfo>,
    ctx: &mut DependencyCollectionContext,
) {
    ctx.enter_function(func);
    for (block_id, block) in &func.body.blocks {
        // Check for scope begin/end at this block
        if !ctx.in_inner_fn
            && let Some(info) = scope_infos.get(block_id)
        {
            match info {
                ScopeBlockInfo::Begin { scope, .. } => {
                    ctx.enter_scope(scope.clone());
                }
                ScopeBlockInfo::End { scope, pruned } => {
                    ctx.exit_scope(scope, *pruned);
                }
            }
        }

        // Process phis: resolve any optional chain temporaries
        for phi in &block.phis {
            for operand in phi.operands.values() {
                if let Some(resolved) = ctx.temporaries.get(&operand.identifier.id) {
                    let dep = ReactiveScopeDependency {
                        identifier: resolved.identifier.clone(),
                        path: resolved.path.clone(),
                    };
                    ctx.visit_dependency(dep);
                }
            }
        }

        // Process instructions
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    ctx.declare(&instr.lvalue.identifier, instr.id);

                    // Recursively process inner function
                    let prev_in_inner = ctx.in_inner_fn;
                    ctx.in_inner_fn = true;
                    collect_deps_from_function(&lowered_func.func, scope_infos, ctx);
                    ctx.in_inner_fn = prev_in_inner;
                }
                _ => {
                    handle_instruction(instr, ctx);
                }
            }
        }

        // Process terminal operands (skip if deferred by optional chain collection)
        if !ctx.is_deferred_dependency_terminal(&block.terminal) {
            crate::hir::visitors::for_each_terminal_operand(&block.terminal, |place| {
                ctx.visit_operand(place);
            });
        }
    }
    ctx.exit_function();
}

/// Process a single instruction for dependency collection.
fn handle_instruction(instr: &Instruction, ctx: &mut DependencyCollectionContext) {
    if std::env::var("DEBUG_SCOPE_TRACE").is_ok() {
        let mut operand_info: Vec<(u32, u32, Option<String>)> = Vec::new();
        crate::hir::visitors::for_each_instruction_operand(instr, |place| {
            operand_info.push((
                place.identifier.id.0,
                place.identifier.declaration_id.0,
                place.identifier.name.as_ref().map(|n| match n {
                    IdentifierName::Named(s) | IdentifierName::Promoted(s) => s.clone(),
                }),
            ));
        });
        eprintln!(
            "[SCOPE_TRACE] instr={} lvalue=(id={},decl={},name={:?}) value={:?} operands={:?}",
            instr.id.0,
            instr.lvalue.identifier.id.0,
            instr.lvalue.identifier.declaration_id.0,
            instr.lvalue.identifier.name,
            instr.value,
            operand_info
        );
    }

    ctx.declare(&instr.lvalue.identifier, instr.id);

    if let InstructionValue::LoadGlobal { binding, .. } = &instr.value {
        ctx.non_local_binding_names
            .insert(instr.lvalue.identifier.id, binding.name().to_string());
    }

    if let InstructionValue::CallExpression { callee, .. } = &instr.value {
        let resolved_callee = ctx.resolve_operand(callee);
        let is_merge_refs_call = ctx
            .dependency_name(&resolved_callee)
            .is_some_and(|name| name == "mergeRefs");
        if is_merge_refs_call {
            ctx.merge_refs_results.insert(instr.lvalue.identifier.id);
        }
    }

    match &instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            let resolved_value = ctx.resolve_operand(value);
            if ctx
                .merge_refs_results
                .contains(&resolved_value.identifier.id)
            {
                ctx.merge_refs_results.insert(lvalue.place.identifier.id);
            }
        }
        _ => {}
    }

    // Skip deferred dependencies
    if ctx.is_deferred_dependency_instr(instr) {
        return;
    }

    match &instr.value {
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            if let JsxTag::Component(component) = tag {
                ctx.visit_operand(component);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { name, place } => {
                        let skip_merge_refs_ref_dep = if name == "ref" {
                            let dep = ctx.resolve_operand(place);
                            ctx.merge_refs_results.contains(&dep.identifier.id)
                        } else {
                            false
                        };
                        if !skip_merge_refs_ref_dep {
                            ctx.visit_operand(place);
                        }
                    }
                    JsxAttribute::SpreadAttribute { argument } => {
                        ctx.visit_operand(argument);
                    }
                }
            }
            if let Some(children) = children {
                for child in children {
                    ctx.visit_operand(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                ctx.visit_operand(child);
            }
        }
        InstructionValue::PropertyLoad {
            object,
            property,
            optional: _,
            ..
        } => {
            ctx.visit_property(object, property, false);
        }
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            ctx.visit_operand(value);
            if lvalue.kind == InstructionKind::Reassign {
                ctx.visit_reassignment(&lvalue.place);
            }
            ctx.declare(&lvalue.place.identifier, instr.id);
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            if !ctx.has_declared(&lvalue.place.identifier)
                || lvalue.kind != InstructionKind::Reassign
            {
                ctx.declare(&lvalue.place.identifier, instr.id);
            }
            // Upstream always visits StoreContext operands via
            // eachInstructionValueOperand, which includes both the lvalue
            // place and the assigned value.
            ctx.visit_operand(&lvalue.place);
            ctx.visit_operand(value);
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            // Only declare non-hoisted declarations
            if !is_hoisted_lvalue_kind(lvalue.kind) {
                ctx.declare(&lvalue.place.identifier, instr.id);
            }
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            ctx.visit_operand(value);
            for_each_pattern_place(&lvalue.pattern, |place| {
                if lvalue.kind == InstructionKind::Reassign {
                    ctx.visit_reassignment(place);
                }
                ctx.declare(&place.identifier, instr.id);
            });
        }
        InstructionValue::CallExpression {
            callee,
            args,
            optional: true,
            ..
        } => {
            ctx.visit_optional_callee_operand(callee);
            for arg in args {
                match arg {
                    Argument::Place(place) | Argument::Spread(place) => {
                        ctx.visit_optional_call_argument(place);
                    }
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional,
            call_optional,
            ..
        } if *receiver_optional || *call_optional => {
            ctx.visit_operand(receiver);
            ctx.visit_optional_callee_operand(property);
            for arg in args {
                match arg {
                    Argument::Place(place) | Argument::Spread(place) => {
                        ctx.visit_conditional_operand(place);
                    }
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional: false,
            call_optional: false,
            ..
        } => {
            let receiver_dep = ctx.resolve_operand(receiver);
            let property_dep = ctx.resolve_operand(property);
            let is_redundant_component_props_receiver = receiver_dep.path.is_empty()
                && !property_dep.path.is_empty()
                && receiver_dep.identifier.declaration_id == property_dep.identifier.declaration_id
                && ctx
                    .component_props_param_decl
                    .is_some_and(|decl| decl == receiver_dep.identifier.declaration_id);
            if !is_redundant_component_props_receiver {
                ctx.visit_dependency(receiver_dep);
            }
            ctx.visit_dependency(property_dep);
            for arg in args {
                match arg {
                    Argument::Place(place) | Argument::Spread(place) => {
                        ctx.visit_operand(place);
                    }
                }
            }
        }
        _other => {
            // Visit all operands
            crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                ctx.visit_operand(place);
            });
        }
    }
}

/// Check if an lvalue kind is hoisted.
/// Corresponds to upstream `convertHoistedLValueKind(kind) === null` — returns true
/// when the kind IS hoisted (i.e. convertHoistedLValueKind would return non-null).
fn is_hoisted_lvalue_kind(kind: InstructionKind) -> bool {
    matches!(
        kind,
        InstructionKind::HoistedConst
            | InstructionKind::HoistedLet
            | InstructionKind::HoistedFunction
    )
}

// ---------------------------------------------------------------------------
// Type checking helpers (port of upstream type predicates)
// ---------------------------------------------------------------------------

/// Check if identifier is a ref value type (e.g. result of accessing ref.current).
/// Upstream: `id.type.kind === 'Object' && id.type.shapeId === 'BuiltInRefValue'`
fn is_ref_value_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInRefValue")
}

/// Check if identifier is an object method type.
/// Upstream: `id.type.kind === 'ObjectMethod'`
fn is_object_method_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::ObjectMethod)
}

/// Check if identifier is a useRef type.
/// Upstream: `id.type.kind === 'Object' && id.type.shapeId === 'BuiltInUseRefId'`
fn is_use_ref_type(id: &Identifier) -> bool {
    matches!(&id.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInUseRefId")
}

fn is_ref_like_identifier(id: &Identifier) -> bool {
    match &id.name {
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            name == "ref" || name.ends_with("Ref")
        }
        _ => false,
    }
}

/// Iterate over all places in a destructuring pattern.
fn for_each_pattern_place(pattern: &Pattern, mut f: impl FnMut(&Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for elem in &arr.items {
                match elem {
                    ArrayElement::Place(p) => f(p),
                    ArrayElement::Spread(p) => f(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => f(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => f(p),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Minimize and apply
// ---------------------------------------------------------------------------

/// Minimize dependencies using ReactiveScopeDependencyTreeHIR and apply to scopes.
///
/// For each scope, creates a dependency tree with the hoistable objects for that scope,
/// adds all collected dependencies, then derives the minimal set. This handles:
/// - Truncating deps to their maximal safe-to-evaluate subpath
/// - Removing child deps subsumed by parent deps (x subsumes x.foo)
/// - Converting optional chains to unconditional when safe
fn minimize_and_apply(
    func: &mut HIRFunction,
    scope_deps: &HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    scope_decls: &HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    scope_reassignments: &HashMap<ScopeId, Vec<Identifier>>,
    scoped_hoistable_loads: &HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
) {
    let mut minimized_deps: HashMap<ScopeId, Vec<ReactiveScopeDependency>> = HashMap::new();
    let debug_scope_deps = std::env::var("DEBUG_SCOPE_DEPS").is_ok();

    for (scope_id, deps) in scope_deps {
        if deps.is_empty() {
            continue;
        }

        // Get hoistable objects for this scope (or use empty set)
        let empty = Vec::new();
        let hoistable = scoped_hoistable_loads.get(scope_id).unwrap_or(&empty);
        if debug_scope_deps {
            let dep_fmt = |dep: &ReactiveScopeDependency| {
                let path = dep
                    .path
                    .iter()
                    .map(|entry| {
                        format!(
                            "{}{}",
                            entry.property,
                            if entry.optional { "?" } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                format!(
                    "id={} decl={} name={:?} path={}",
                    dep.identifier.id.0, dep.identifier.declaration_id.0, dep.identifier.name, path
                )
            };
            eprintln!(
                "[SCOPE_DEP_MIN] scope={} raw_deps={} hoistable={}",
                scope_id.0,
                deps.iter().map(dep_fmt).collect::<Vec<_>>().join(" | "),
                hoistable
                    .iter()
                    .map(dep_fmt)
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }

        let mut tree = crate::hir::derive_minimal_dependencies::ReactiveScopeDependencyTreeHIR::new(
            hoistable.iter().cloned(),
        );
        for dep in deps {
            tree.add_dependency(dep);
        }
        let mut minimal = tree.derive_minimal_dependencies();

        // Deduplicate by declaration_id + path (matching upstream
        // PropagateScopeDependenciesHIR.ts:112-123 which checks
        // existingDep.identifier.declarationId === candidateDep.identifier.declarationId
        // && areEqualPaths).
        {
            let mut seen: HashSet<(DeclarationId, Vec<(String, bool)>)> = HashSet::new();
            minimal.retain(|dep| {
                let key = (
                    dep.identifier.declaration_id,
                    dep.path
                        .iter()
                        .map(|e| (e.property.clone(), e.optional))
                        .collect::<Vec<_>>(),
                );
                seen.insert(key)
            });
        }

        // Sort deps deterministically by identifier ID then path.
        // Upstream JS Map preserves insertion order; Rust HashMap does not.
        minimal.sort_by(|a, b| {
            a.identifier
                .id
                .cmp(&b.identifier.id)
                .then_with(|| a.path.len().cmp(&b.path.len()))
                .then_with(|| {
                    for (ae, be) in a.path.iter().zip(b.path.iter()) {
                        let c = ae.property.cmp(&be.property);
                        if c != std::cmp::Ordering::Equal {
                            return c;
                        }
                    }
                    std::cmp::Ordering::Equal
                })
        });
        if debug_scope_deps {
            eprintln!(
                "[SCOPE_DEP_MIN] scope={} minimal={}",
                scope_id.0,
                minimal
                    .iter()
                    .map(|dep| {
                        let path = dep
                            .path
                            .iter()
                            .map(|entry| {
                                format!(
                                    "{}{}",
                                    entry.property,
                                    if entry.optional { "?" } else { "" }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(".");
                        format!(
                            "id={} decl={} name={:?} path={}",
                            dep.identifier.id.0,
                            dep.identifier.declaration_id.0,
                            dep.identifier.name,
                            path
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        minimized_deps.insert(*scope_id, minimal);
    }

    // Apply to all scope instances on identifiers
    apply_to_scopes(func, &minimized_deps, scope_decls, scope_reassignments);
}

/// Apply collected dependencies and declarations to scopes on identifiers.
fn apply_to_scopes(
    func: &mut HIRFunction,
    scope_deps: &HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    scope_decls: &HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    scope_reassignments: &HashMap<ScopeId, Vec<Identifier>>,
) {
    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            update_identifier_scope(
                &mut instr.lvalue.identifier,
                scope_deps,
                scope_decls,
                scope_reassignments,
            );

            match &mut instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    update_identifier_scope(
                        &mut place.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    update_identifier_scope(
                        &mut lvalue.place.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                    update_identifier_scope(
                        &mut value.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    update_identifier_scope(
                        &mut lvalue.place.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                    update_identifier_scope(
                        &mut value.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                }
                InstructionValue::PropertyLoad { object, .. } => {
                    update_identifier_scope(
                        &mut object.identifier,
                        scope_deps,
                        scope_decls,
                        scope_reassignments,
                    );
                }
                _ => {}
            }
        }

        // Also update terminal operand scopes
        match &mut block.terminal {
            Terminal::Scope { scope, .. } | Terminal::PrunedScope { scope, .. } => {
                if let Some(deps) = scope_deps.get(&scope.id) {
                    scope.dependencies = deps.clone();
                }
                if let Some(decls) = scope_decls.get(&scope.id) {
                    scope.declarations = decls.clone();
                }
                if let Some(reassignments) = scope_reassignments.get(&scope.id) {
                    scope.reassignments = reassignments.clone();
                }
            }
            _ => {}
        }
    }
}

/// Update a single identifier's scope with collected data.
fn update_identifier_scope(
    identifier: &mut Identifier,
    deps: &HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    decls: &HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    reassignments: &HashMap<ScopeId, Vec<Identifier>>,
) {
    if let Some(scope) = &mut identifier.scope {
        let scope_id = scope.id;
        if let Some(d) = deps.get(&scope_id) {
            scope.dependencies = d.clone();
        }
        if let Some(d) = decls.get(&scope_id) {
            scope.declarations = d.clone();
        }
        if let Some(r) = reassignments.get(&scope_id) {
            scope.reassignments = r.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

    #[test]
    fn test_scope_block_info_detection() {
        let scope = ReactiveScope {
            id: ScopeId(1),
            range: MutableRange {
                start: InstructionId(1),
                end: InstructionId(5),
            },
            dependencies: vec![],
            declarations: IndexMap::new(),
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        };

        let blocks = vec![
            (
                BlockId(0),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(0),
                    instructions: vec![],
                    terminal: Terminal::Scope {
                        block: BlockId(1),
                        fallthrough: BlockId(2),
                        scope: scope.clone(),
                        id: InstructionId(0),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(1),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(1),
                    instructions: vec![],
                    terminal: Terminal::Goto {
                        block: BlockId(2),
                        variant: GotoVariant::Break,
                        id: InstructionId(1),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
            (
                BlockId(2),
                BasicBlock {
                    kind: BlockKind::Block,
                    id: BlockId(2),
                    instructions: vec![],
                    terminal: Terminal::Return {
                        value: make_place(99, None),
                        return_variant: ReturnVariant::Explicit,
                        id: InstructionId(2),
                        loc: SourceLocation::Generated,
                    },
                    preds: HashSet::new(),
                    phis: vec![],
                },
            ),
        ];

        let func = HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Component,
            params: vec![],
            returns: make_place(99, None),
            context: vec![],
            body: HIR {
                entry: BlockId(0),
                blocks,
            },
            generator: false,
            async_: false,
            directives: vec![],
            aliasing_effects: None,
        };

        let infos = build_scope_block_infos(&func);

        // Block 1 should be Begin, Block 2 should be End
        assert!(matches!(
            infos.get(&BlockId(1)),
            Some(ScopeBlockInfo::Begin { pruned: false, .. })
        ));
        assert!(matches!(
            infos.get(&BlockId(2)),
            Some(ScopeBlockInfo::End { pruned: false, .. })
        ));
    }

    #[test]
    fn test_temporaries_resolution() {
        let _used_outside: HashSet<DeclarationId> = HashSet::new();
        let mut temporaries: HashMap<IdentifierId, ResolvedDep> = HashMap::new();

        // Simulate: $0 = LoadLocal 'x' (mapped to {identifier: x, path: []})
        let x_ident = make_identifier(100, Some("x"));
        temporaries.insert(
            IdentifierId(0),
            ResolvedDep {
                identifier: x_ident.clone(),
                path: vec![],
            },
        );

        // get_property($0, "a", false) should give {identifier: x, path: [a]}
        let obj = make_place(0, None);
        let result = get_property(
            &obj,
            &PropertyLiteral::String("a".to_string()),
            false,
            &temporaries,
        );

        assert_eq!(result.identifier.id, IdentifierId(100));
        assert_eq!(result.path.len(), 1);
        assert_eq!(result.path[0].property, "a");
    }

    // Subsumption (x subsumes x.foo) is now handled by
    // ReactiveScopeDependencyTreeHIR and tested in derive_minimal_dependencies.rs
}
