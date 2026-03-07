//! Port of PropagateScopeDependenciesHIR from upstream React Compiler.
//!
//! This pass populates `scope.dependencies` and `scope.declarations` for each
//! reactive scope. It determines:
//! - Which identifiers each scope depends on (declared before the scope)
//! - Which identifiers a scope declares that are used outside it
//!
//! Simplified implementation that uses scope ranges instead of scope terminals
//! since we don't have `buildReactiveScopeTerminalsHIR` yet.
//!
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::hir::types::*;

/// A resolved dependency: base identifier + property path.
#[derive(Debug, Clone)]
struct ResolvedDep {
    identifier: Identifier,
    path: Vec<DependencyPathEntry>,
}

/// Where a declaration was first seen.
#[derive(Debug, Clone)]
struct DeclInfo {
    /// Instruction ID where it was declared.
    instr_id: InstructionId,
    /// Which scope it was declared in (if any).
    scope_id: Option<ScopeId>,
}

/// Main entry point: propagate scope dependencies through the HIR.
pub fn propagate_scope_dependencies(func: &mut HIRFunction) {
    // Phase 1: Collect all unique scopes from identifiers.
    let scopes = collect_scopes(func);
    if scopes.is_empty() {
        return;
    }

    // Phase 2: Build temporaries sidemap — resolve PropertyLoad chains
    // to their base identifier + property path.
    let temporaries = collect_temporaries(func, &scopes);

    // Phase 3: Collect dependencies for each scope.
    let (mut scope_deps, scope_decls, scope_reassignments) =
        collect_dependencies(func, &scopes, &temporaries);

    // Phase 3.5: Prune overlapping dependencies.
    // When both a parent (e.g., `props`) and a child (e.g., `props.cond`) exist
    // as dependencies for the same scope, keep only the parent.
    // This matches upstream's pruneNonReactiveDependencies behavior.
    for deps in scope_deps.values_mut() {
        prune_overlapping_deps(deps);
    }

    // Phase 4: Apply collected dependencies and declarations to scopes.
    apply_to_scopes(func, &scope_deps, &scope_decls, &scope_reassignments);
}

/// Scope info extracted from identifiers.
#[derive(Debug, Clone)]
struct ScopeInfo {
    id: ScopeId,
    range: MutableRange,
}

type ScopeDependencyMaps = (
    HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    HashMap<ScopeId, Vec<Identifier>>,
);

/// Collect all unique scopes from identifiers in the function.
fn collect_scopes(func: &HIRFunction) -> Vec<ScopeInfo> {
    let mut seen: HashSet<ScopeId> = HashSet::new();
    let mut scopes: Vec<ScopeInfo> = Vec::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope
                && seen.insert(scope.id)
            {
                scopes.push(ScopeInfo {
                    id: scope.id,
                    range: scope.range.clone(),
                });
            }
            // Also check operand scopes
            crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                if let Some(scope) = &place.identifier.scope
                    && seen.insert(scope.id)
                {
                    scopes.push(ScopeInfo {
                        id: scope.id,
                        range: scope.range.clone(),
                    });
                }
            });
        }
    }

    // Sort by range start for deterministic ordering.
    scopes.sort_by_key(|s| s.range.start);
    scopes
}

/// Determine which scope (if any) an instruction belongs to based on its ID.
fn find_scope_for_instr(instr_id: InstructionId, scopes: &[ScopeInfo]) -> Option<ScopeId> {
    // Find the innermost scope that contains this instruction.
    // Scopes are sorted by start, so we find the last one that starts <= instr_id
    // and ends > instr_id.
    let mut best: Option<&ScopeInfo> = None;
    for scope in scopes {
        if instr_id >= scope.range.start && instr_id < scope.range.end {
            match best {
                None => best = Some(scope),
                Some(b) => {
                    // Prefer the innermost (narrowest) scope
                    let b_size = b.range.end.0 - b.range.start.0;
                    let s_size = scope.range.end.0 - scope.range.start.0;
                    if s_size < b_size {
                        best = Some(scope);
                    }
                }
            }
        }
    }
    best.map(|s| s.id)
}

/// Build a map of temporary IDs to their resolved dependency (base identifier + path).
/// This resolves chains like:
///   $0 = LoadLocal 'a'           → {identifier: a, path: []}
///   $1 = PropertyLoad $0.b       → {identifier: a, path: [{property: "b"}]}
///   $2 = PropertyLoad $1.c       → {identifier: a, path: [{property: "b"}, {property: "c"}]}
fn collect_temporaries(
    func: &HIRFunction,
    _scopes: &[ScopeInfo],
) -> HashMap<IdentifierId, ResolvedDep> {
    let mut temporaries: HashMap<IdentifierId, ResolvedDep> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    // Only track unnamed temporaries (named vars are used directly)
                    if instr.lvalue.identifier.name.is_none() && place.identifier.name.is_some() {
                        temporaries.insert(
                            instr.lvalue.identifier.id,
                            ResolvedDep {
                                identifier: place.identifier.clone(),
                                path: Vec::new(),
                            },
                        );
                    }
                }
                InstructionValue::PropertyLoad {
                    object,
                    property,
                    optional,
                    ..
                } => {
                    // Try to resolve through temporaries chain
                    if let Some(base) = temporaries.get(&object.identifier.id) {
                        let mut path = base.path.clone();
                        match property {
                            PropertyLiteral::String(s) => {
                                path.push(DependencyPathEntry {
                                    property: s.clone(),
                                    optional: *optional,
                                });
                            }
                            PropertyLiteral::Number(n) => {
                                path.push(DependencyPathEntry {
                                    property: n.to_string(),
                                    optional: *optional,
                                });
                            }
                        }
                        temporaries.insert(
                            instr.lvalue.identifier.id,
                            ResolvedDep {
                                identifier: base.identifier.clone(),
                                path,
                            },
                        );
                    } else if object.identifier.name.is_some() {
                        // Direct property load from named variable
                        let mut path = Vec::new();
                        match property {
                            PropertyLiteral::String(s) => {
                                path.push(DependencyPathEntry {
                                    property: s.clone(),
                                    optional: *optional,
                                });
                            }
                            PropertyLiteral::Number(n) => {
                                path.push(DependencyPathEntry {
                                    property: n.to_string(),
                                    optional: *optional,
                                });
                            }
                        }
                        temporaries.insert(
                            instr.lvalue.identifier.id,
                            ResolvedDep {
                                identifier: object.identifier.clone(),
                                path,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
    }

    temporaries
}

/// Resolve a place to its dependency (using temporaries if available).
fn resolve_place(place: &Place, temporaries: &HashMap<IdentifierId, ResolvedDep>) -> ResolvedDep {
    if let Some(resolved) = temporaries.get(&place.identifier.id) {
        resolved.clone()
    } else {
        ResolvedDep {
            identifier: place.identifier.clone(),
            path: Vec::new(),
        }
    }
}

/// Collect dependencies for each scope by traversing the HIR.
fn collect_dependencies(
    func: &HIRFunction,
    scopes: &[ScopeInfo],
    temporaries: &HashMap<IdentifierId, ResolvedDep>,
) -> ScopeDependencyMaps {
    let mut scope_deps: HashMap<ScopeId, Vec<ReactiveScopeDependency>> = HashMap::new();
    let mut scope_decls: HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>> =
        HashMap::new();
    let mut scope_reassignments: HashMap<ScopeId, Vec<Identifier>> = HashMap::new();

    // Track where each identifier was declared (instruction ID + scope)
    let mut declarations: HashMap<IdentifierId, DeclInfo> = HashMap::new();
    // Also track by declaration_id for SSA-renamed identifiers
    let mut decl_by_declaration_id: HashMap<DeclarationId, DeclInfo> = HashMap::new();

    // Register function parameters as declared at instruction 0 (before any scope)
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        let info = DeclInfo {
            instr_id: InstructionId(0),
            scope_id: None,
        };
        declarations.insert(place.identifier.id, info.clone());
        decl_by_declaration_id.insert(place.identifier.declaration_id, info);
    }

    // Build scope lookup for quick access
    let scope_map: HashMap<ScopeId, &ScopeInfo> = scopes.iter().map(|s| (s.id, s)).collect();

    // First pass: record all declarations
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let instr_scope = find_scope_for_instr(instr.id, scopes);

            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    let info = DeclInfo {
                        instr_id: instr.id,
                        scope_id: instr_scope,
                    };
                    declarations.insert(lvalue.place.identifier.id, info.clone());
                    decl_by_declaration_id.insert(lvalue.place.identifier.declaration_id, info);
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    // Only record first store per IdentifierId
                    if let std::collections::hash_map::Entry::Vacant(e) =
                        declarations.entry(lvalue.place.identifier.id)
                    {
                        let info = DeclInfo {
                            instr_id: instr.id,
                            scope_id: instr_scope,
                        };
                        e.insert(info.clone());
                        // Use entry().or_insert() to preserve the EARLIEST declaration
                        // for this declaration_id. After SSA, multiple IdentifierIds share
                        // the same declaration_id but we want the original DeclareLocal,
                        // not an SSA-renamed StoreLocal that may be inside a scope.
                        decl_by_declaration_id
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_insert(info);
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    // Register pattern outputs
                    for_each_pattern_place(&lvalue.pattern, |place| {
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            declarations.entry(place.identifier.id)
                        {
                            let info = DeclInfo {
                                instr_id: instr.id,
                                scope_id: instr_scope,
                            };
                            e.insert(info.clone());
                            // Preserve earliest declaration per declaration_id
                            decl_by_declaration_id
                                .entry(place.identifier.declaration_id)
                                .or_insert(info);
                        }
                    });
                }
                // The lvalue of every instruction is also a declaration
                _ => {
                    declarations
                        .entry(instr.lvalue.identifier.id)
                        .or_insert_with(|| DeclInfo {
                            instr_id: instr.id,
                            scope_id: instr_scope,
                        });
                }
            }
        }
    }

    // Second pass: collect dependencies and declarations
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let instr_scope_id = find_scope_for_instr(instr.id, scopes);
            let instr_scope = instr_scope_id.and_then(|id| scope_map.get(&id));

            if instr_scope.is_none() {
                continue; // Not inside any scope
            }
            let scope = *instr_scope.unwrap();

            // Skip deferred dependencies (temporaries that will be resolved at use-site)
            if temporaries.contains_key(&instr.lvalue.identifier.id) {
                // Still need to declare the lvalue
                continue;
            }

            // Visit operands to collect dependencies
            let mut operands: Vec<Place> = Vec::new();
            match &instr.value {
                InstructionValue::PropertyLoad { object, .. } => {
                    // PropertyLoad: resolve to full path
                    operands.push(object.clone());
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    operands.push(value.clone());
                    // Track reassignments
                    if lvalue.kind == InstructionKind::Reassign {
                        visit_reassignment(
                            &lvalue.place,
                            scope,
                            &declarations,
                            &decl_by_declaration_id,
                            &mut scope_reassignments,
                        );
                    }
                }
                InstructionValue::StoreContext { value, .. } => {
                    operands.push(value.clone());
                }
                InstructionValue::Destructure { value, lvalue, .. } => {
                    operands.push(value.clone());
                    // Check for reassignment in destructure
                    if lvalue.kind == InstructionKind::Reassign {
                        for_each_pattern_place(&lvalue.pattern, |place| {
                            visit_reassignment(
                                place,
                                scope,
                                &declarations,
                                &decl_by_declaration_id,
                                &mut scope_reassignments,
                            );
                        });
                    }
                }
                _ => {
                    // Collect all operands
                    crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                        operands.push(place.clone());
                    });
                }
            }

            for operand in &operands {
                let dep = resolve_place(operand, temporaries);
                visit_dependency(
                    &dep,
                    scope,
                    &declarations,
                    &decl_by_declaration_id,
                    &scope_map,
                    &mut scope_deps,
                    &mut scope_decls,
                );
            }
        }

        // Also process terminal operands
        let terminal_instr_id = block.terminal.id();
        let term_scope_id = find_scope_for_instr(terminal_instr_id, scopes);
        let term_scope = term_scope_id.and_then(|id| scope_map.get(&id));

        if let Some(scope) = term_scope {
            match &block.terminal {
                Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                    let dep = resolve_place(value, temporaries);
                    visit_dependency(
                        &dep,
                        scope,
                        &declarations,
                        &decl_by_declaration_id,
                        &scope_map,
                        &mut scope_deps,
                        &mut scope_decls,
                    );
                }
                Terminal::If { test, .. }
                | Terminal::Branch { test, .. }
                | Terminal::Switch { test, .. } => {
                    let dep = resolve_place(test, temporaries);
                    visit_dependency(
                        &dep,
                        scope,
                        &declarations,
                        &decl_by_declaration_id,
                        &scope_map,
                        &mut scope_deps,
                        &mut scope_decls,
                    );
                }
                _ => {}
            }
        }
    }

    (scope_deps, scope_decls, scope_reassignments)
}

/// Visit a dependency: add to scope's dependencies if valid, and populate
/// scope declarations if the value was created in another scope.
fn visit_dependency(
    dep: &ResolvedDep,
    scope: &ScopeInfo,
    declarations: &HashMap<IdentifierId, DeclInfo>,
    decl_by_declaration_id: &HashMap<DeclarationId, DeclInfo>,
    _scope_map: &HashMap<ScopeId, &ScopeInfo>,
    scope_deps: &mut HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    scope_decls: &mut HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
) {
    // Find where this identifier was declared
    let decl = declarations
        .get(&dep.identifier.id)
        .or_else(|| decl_by_declaration_id.get(&dep.identifier.declaration_id));
    let decl = match decl {
        Some(d) => d,
        None => return, // Unknown identifier, skip
    };

    // Populate scope.declarations: if the value was declared in a different scope
    // and that scope is not the current scope, add to that scope's declarations.
    if let Some(decl_scope_id) = decl.scope_id
        && decl_scope_id != scope.id
    {
        // The value was declared in another scope — it must be output by that scope
        let decl_map = scope_decls.entry(decl_scope_id).or_default();
        decl_map
            .entry(dep.identifier.id)
            .or_insert_with(|| ScopeDeclaration {
                identifier: dep.identifier.clone(),
                scope: make_declaration_scope(decl_scope_id),
            });
    }

    // Check if this is a valid dependency (declared before scope starts).
    // Matches upstream's #checkValidDependency: only checks declaration position.
    // Reactivity filtering is deferred to prune_non_reactive_dependencies pass.
    if decl.instr_id >= scope.range.start {
        return; // Declared inside this scope, not a dependency
    }

    // Add as dependency (avoid duplicates)
    let deps = scope_deps.entry(scope.id).or_default();

    // Check for duplicate: same declaration_id + same path
    let is_dup = deps.iter().any(|d| {
        d.identifier.declaration_id == dep.identifier.declaration_id
            && d.path.len() == dep.path.len()
            && d.path
                .iter()
                .zip(dep.path.iter())
                .all(|(a, b)| a.property == b.property)
    });

    if !is_dup {
        deps.push(ReactiveScopeDependency {
            identifier: dep.identifier.clone(),
            path: dep.path.clone(),
        });
    }
}

/// Track reassignment of a place in its scope.
fn visit_reassignment(
    place: &Place,
    scope: &ScopeInfo,
    declarations: &HashMap<IdentifierId, DeclInfo>,
    decl_by_declaration_id: &HashMap<DeclarationId, DeclInfo>,
    scope_reassignments: &mut HashMap<ScopeId, Vec<Identifier>>,
) {
    // Prefer decl_by_declaration_id — it maps to the ORIGINAL declaration (DeclareLocal)
    // which was before the scope. The `declarations` map may instead find an SSA-renamed
    // StoreLocal/Reassign whose instr_id is inside the scope, making us miss the reassignment.
    let decl = decl_by_declaration_id
        .get(&place.identifier.declaration_id)
        .or_else(|| declarations.get(&place.identifier.id));
    if let Some(decl) = decl
        && decl.instr_id < scope.range.start
    {
        let reassignments = scope_reassignments.entry(scope.id).or_default();
        if !reassignments
            .iter()
            .any(|r| r.declaration_id == place.identifier.declaration_id)
        {
            reassignments.push(place.identifier.clone());
        }
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

/// Prune overlapping dependencies: when both a parent dep (e.g., `props`)
/// and a child dep (e.g., `props.cond`) exist for the same scope, keep only
/// the parent. This is because if `props` changes, the scope needs to re-execute
/// regardless — so checking `props.cond` is redundant.
///
/// Port of upstream's `pruneNonReactiveDependencies` behavior.
fn prune_overlapping_deps(deps: &mut Vec<ReactiveScopeDependency>) {
    if deps.len() <= 1 {
        return;
    }

    let mut to_remove: HashSet<usize> = HashSet::new();

    for i in 0..deps.len() {
        if to_remove.contains(&i) {
            continue;
        }
        for j in 0..deps.len() {
            if i == j || to_remove.contains(&j) {
                continue;
            }
            // Check if dep[i] is a parent of dep[j] (same base identifier, i's path is a prefix of j's path)
            if deps[i].identifier.declaration_id == deps[j].identifier.declaration_id
                && deps[i].path.len() < deps[j].path.len()
                && deps[i]
                    .path
                    .iter()
                    .zip(deps[j].path.iter())
                    .all(|(a, b)| a.property == b.property)
            {
                // dep[i] is a prefix of dep[j] — remove dep[j]
                to_remove.insert(j);
            }
        }
    }

    if !to_remove.is_empty() {
        let mut idx = 0;
        deps.retain(|_| {
            let keep = !to_remove.contains(&idx);
            idx += 1;
            keep
        });
    }
}

/// Apply collected dependencies and declarations to the actual scopes on identifiers.
fn apply_to_scopes(
    func: &mut HIRFunction,
    scope_deps: &HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    scope_decls: &HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>>,
    scope_reassignments: &HashMap<ScopeId, Vec<Identifier>>,
) {
    // We need to update the scope data on every identifier that has a scope.
    // Since scopes are cloned on each identifier, we need to update all of them.
    // First, build the final scope data.
    let mut final_deps: HashMap<ScopeId, Vec<ReactiveScopeDependency>> = HashMap::new();
    let mut final_decls: HashMap<ScopeId, IndexMap<IdentifierId, ScopeDeclaration>> =
        HashMap::new();
    let mut final_reassignments: HashMap<ScopeId, Vec<Identifier>> = HashMap::new();

    for (scope_id, deps) in scope_deps {
        final_deps.insert(*scope_id, deps.clone());
    }
    for (scope_id, decls) in scope_decls {
        final_decls.insert(*scope_id, decls.clone());
    }
    for (scope_id, reassignments) in scope_reassignments {
        final_reassignments.insert(*scope_id, reassignments.clone());
    }

    // Update all identifiers that have scopes
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            update_identifier_scope(
                &mut instr.lvalue.identifier,
                &final_deps,
                &final_decls,
                &final_reassignments,
            );

            // Update operand identifiers too
            match &mut instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    update_identifier_scope(
                        &mut place.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    update_identifier_scope(
                        &mut lvalue.place.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                    update_identifier_scope(
                        &mut value.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    update_identifier_scope(
                        &mut lvalue.place.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                    update_identifier_scope(
                        &mut value.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                }
                InstructionValue::PropertyLoad { object, .. } => {
                    update_identifier_scope(
                        &mut object.identifier,
                        &final_deps,
                        &final_decls,
                        &final_reassignments,
                    );
                }
                // For other instruction types, we'd need to handle all variants.
                // For now, skip — the scope data is primarily used by codegen
                // which reads from the lvalue's scope.
                _ => {}
            }
        }
    }
}

/// Update a single identifier's scope with collected dependency/declaration data.
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
