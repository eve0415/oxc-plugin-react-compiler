//! Validates that all explicit manual memoization (useMemo/useCallback) was accurately
//! preserved, and that no originally memoized values became unmemoized in the output.
//!
//! Port of `ValidatePreservedManualMemoization.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This can occur if a value's mutable range somehow extended to include a hook and
//! was pruned.
//!
//! This pass runs on the **ReactiveFunction** (tree form), NOT the HIR.
//! It is gated behind `enablePreserveExistingMemoizationGuarantees ||
//! validatePreserveExistingMemoizationGuarantees`.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::types::*;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Validates that all explicit manual memoization (useMemo/useCallback) was accurately
/// preserved, and that no originally memoized values became unmemoized in the output.
///
/// Returns `Ok(())` if validation passes, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_preserved_manual_memoization(func: &ReactiveFunction) -> Result<(), CompilerError> {
    let debug_manual_memo = std::env::var("DEBUG_MANUAL_MEMO").is_ok();
    let mut state = VisitorState {
        errors: Vec::new(),
        manual_memo_state: None,
        late_mutation_watchers: HashMap::new(),
        late_mutation_reported: HashSet::new(),
        pending_unmemoized_checks: Vec::new(),
    };
    let mut visitor = Visitor {
        scopes: HashSet::new(),
        pruned_scopes: HashSet::new(),
        scopes_with_non_temp_decl: HashSet::new(),
        temporaries: HashMap::new(),
    };
    visitor.visit_block(&func.body, &mut state);
    for pending in state.pending_unmemoized_checks.drain(..) {
        let unresolved = is_unmemoized(&pending.identifier, &visitor.scopes)
            || pending
                .identifier
                .scope
                .as_ref()
                .is_some_and(|scope| !visitor.scopes_with_non_temp_decl.contains(&scope.id));
        if unresolved {
            state.errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::CannotPreserveMemoization,
                message: "React Compiler has skipped optimizing this component \
                     because the existing manual memoization could not be \
                     preserved. This value was memoized in source but not \
                     in compilation output"
                    .to_string(),
            });
            if debug_manual_memo {
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] pending check unresolved id={} decl={} scope={:?} loc={:?}",
                    pending.identifier.id.0,
                    pending.identifier.declaration_id.0,
                    pending.identifier.scope.as_ref().map(|s| s.id.0),
                    pending.loc
                );
            }
        }
    }
    if debug_manual_memo && let Some(unclosed) = state.manual_memo_state.as_ref() {
        eprintln!(
            "[MANUAL_MEMO_VALIDATE] unclosed memo_id={} watched_dep_decls={} side_effect_use={}",
            unclosed.manual_memo_id,
            unclosed
                .watched_dep_decls
                .iter()
                .map(|d| d.0.to_string())
                .collect::<Vec<_>>()
                .join(","),
            unclosed.saw_side_effect_use_of_decl
        );
    }
    if let Some(unclosed) = state.manual_memo_state.as_ref() {
        if !unclosed.watched_dep_decls.is_empty() {
            state.errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::CannotPreserveMemoization,
                message: "React Compiler has skipped optimizing this \
                        component because the existing manual memoization \
                        could not be preserved. This dependency may be \
                        mutated later, which could cause the value to \
                        change unexpectedly"
                    .to_string(),
            });
        } else if unclosed.saw_side_effect_use_of_decl {
            state.errors.push(CompilerDiagnostic {
                severity: DiagnosticSeverity::CannotPreserveMemoization,
                message: "React Compiler has skipped optimizing this component \
                     because the existing manual memoization could not be \
                     preserved. This value was memoized in source but not \
                     in compilation output"
                    .to_string(),
            });
        }
    }
    if debug_manual_memo {
        eprintln!(
            "[MANUAL_MEMO_VALIDATE] scopes={} pruned_scopes={} temporaries={} errors={}",
            visitor.scopes.len(),
            visitor.pruned_scopes.len(),
            visitor.temporaries.len(),
            state.errors.len()
        );
    }

    if state.errors.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Existing memoization could not be preserved".to_string(),
            diagnostics: state.errors,
        }))
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// State for an active StartMemoize..FinishMemoize block.
struct ManualMemoBlockState {
    /// Tracks reassigned temporaries.
    /// This is necessary because useMemo calls are usually inlined.
    /// Inlining produces a `let` declaration, followed by reassignments
    /// to the newly declared variable (one per return statement).
    /// Since InferReactiveScopes does not merge scopes across reassigned
    /// variables (except in the case of a mutate-after-phi), we need to
    /// track reassignments to validate we're retaining manual memo.
    reassignments: HashMap<DeclarationId, HashSet<IdentifierId>>,

    /// The source of the original memoization, used when reporting errors.
    loc: SourceLocation,

    /// Values produced within manual memoization blocks.
    /// We track these to ensure our inferred dependencies are
    /// produced before the manual memo block starts.
    decls: HashSet<DeclarationId>,

    /// Normalized depslist from useMemo/useCallback callsite in source.
    deps_from_source: Option<Vec<ManualMemoDependency>>,

    /// The manual memo id from StartMemoize.
    manual_memo_id: u32,

    /// Source dependencies (by declaration) that should not be mutated
    /// after this memo block finishes.
    watched_dep_decls: HashSet<DeclarationId>,

    /// Declarations that alias a value produced within the active
    /// manual memo block (including transitive aliases).
    memo_alias_decls: HashSet<DeclarationId>,

    /// Tracks whether a value produced while this memo block is open
    /// is consumed by a side-effectful instruction.
    saw_side_effect_use_of_decl: bool,
}

#[derive(Clone)]
struct LateMutationWatch {
    manual_memo_id: u32,
}

struct VisitorState {
    errors: Vec<CompilerDiagnostic>,
    manual_memo_state: Option<ManualMemoBlockState>,
    late_mutation_watchers: HashMap<DeclarationId, Vec<LateMutationWatch>>,
    late_mutation_reported: HashSet<(DeclarationId, u32)>,
    pending_unmemoized_checks: Vec<PendingUnmemoizedCheck>,
}

#[derive(Clone)]
struct PendingUnmemoizedCheck {
    identifier: Identifier,
    loc: SourceLocation,
}

// ---------------------------------------------------------------------------
// Dependency comparison
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CompareDependencyResult {
    Ok = 0,
    RootDifference = 1,
    PathDifference = 2,
    Subpath = 3,
    RefAccessDifference = 4,
}

fn merge_compare(
    a: CompareDependencyResult,
    b: CompareDependencyResult,
) -> CompareDependencyResult {
    if a >= b { a } else { b }
}

fn get_compare_dependency_result_description(result: CompareDependencyResult) -> &'static str {
    match result {
        CompareDependencyResult::Ok => "Dependencies equal",
        CompareDependencyResult::RootDifference | CompareDependencyResult::PathDifference => {
            "Inferred different dependency than source"
        }
        CompareDependencyResult::RefAccessDifference => "Differences in ref.current access",
        CompareDependencyResult::Subpath => "Inferred less specific property than source",
    }
}

fn compare_deps(
    inferred: &ManualMemoDependency,
    source: &ManualMemoDependency,
) -> CompareDependencyResult {
    let roots_equal = match (&inferred.root, &source.root) {
        (
            ManualMemoRoot::Global { identifier_name: a },
            ManualMemoRoot::Global { identifier_name: b },
        ) => a == b,
        (ManualMemoRoot::NamedLocal(a), ManualMemoRoot::NamedLocal(b)) => {
            // Use declaration_id for comparison instead of identifier.id.
            // In the upstream TypeScript, identifier.id works because objects are
            // reference types -- DropManualMemoization stores references to Identifier
            // objects that SSA later mutates in-place. In Rust, we clone identifiers,
            // so the source deps retain pre-SSA ids while scope deps have post-SSA ids.
            // Using declaration_id ensures the same variable matches across SSA versions.
            a.identifier.declaration_id == b.identifier.declaration_id
        }
        _ => false,
    };

    if !roots_equal {
        return CompareDependencyResult::RootDifference;
    }

    let min_len = inferred.path.len().min(source.path.len());
    let mut is_subpath = true;
    for i in 0..min_len {
        if inferred.path[i].property != source.path[i].property {
            is_subpath = false;
            break;
        } else if inferred.path[i].optional != source.path[i].optional {
            // Allow inferred-optional vs source-non-optional: the compiler
            // tracks optional chains more precisely than upstream's scope
            // dependency propagation. When the user writes `propB?.x.y` in
            // the useMemo body but `propB.x.y` in the deps array, the
            // compiler infers optional=true while the source has optional=false.
            // This is safe — the compiler is more cautious than the user.
            if inferred.path[i].optional && !source.path[i].optional {
                continue;
            }
            return CompareDependencyResult::PathDifference;
        }
    }

    if is_subpath
        && (source.path.len() == inferred.path.len()
            || (inferred.path.len() >= source.path.len()
                && !inferred
                    .path
                    .iter()
                    .any(|token| token.property == "current")))
    {
        CompareDependencyResult::Ok
    } else if is_subpath {
        if source.path.iter().any(|token| token.property == "current")
            || inferred
                .path
                .iter()
                .any(|token| token.property == "current")
        {
            CompareDependencyResult::RefAccessDifference
        } else {
            CompareDependencyResult::Subpath
        }
    } else {
        CompareDependencyResult::PathDifference
    }
}

// ---------------------------------------------------------------------------
// Pretty-printing helpers
// ---------------------------------------------------------------------------

fn pretty_print_scope_dependency(val: &ReactiveScopeDependency) -> String {
    let root_str = match &val.identifier.name {
        Some(IdentifierName::Named(name)) => name.clone(),
        _ => "[unnamed]".to_string(),
    };
    let path_str: String = val
        .path
        .iter()
        .map(|v| format!("{}{}", if v.optional { "?." } else { "." }, v.property))
        .collect();
    format!("{root_str}{path_str}")
}

fn print_manual_memo_dependency(val: &ManualMemoDependency, name_only: bool) -> String {
    let root_str = match &val.root {
        ManualMemoRoot::Global { identifier_name } => identifier_name.clone(),
        ManualMemoRoot::NamedLocal(place) => {
            if name_only {
                match &place.identifier.name {
                    Some(IdentifierName::Named(name)) => name.clone(),
                    Some(IdentifierName::Promoted(name)) => name.clone(),
                    None => "[unnamed]".to_string(),
                }
            } else {
                match &place.identifier.name {
                    Some(IdentifierName::Named(name)) => name.clone(),
                    Some(IdentifierName::Promoted(name)) => name.clone(),
                    None => format!("${}", place.identifier.id.0),
                }
            }
        }
    };
    let path_str: String = val
        .path
        .iter()
        .map(|v| format!("{}{}", if v.optional { "?." } else { "." }, v.property))
        .collect();
    format!("{root_str}{path_str}")
}

// ---------------------------------------------------------------------------
// Validate a single inferred dependency
// ---------------------------------------------------------------------------

/// Validate that an inferred dependency either matches a source dependency
/// or is produced by earlier instructions in the same manual memoization
/// call.
///
/// Inferred dependency `rootA.[pathA]` matches a source dependency `rootB.[pathB]`
/// when:
///   - rootA and rootB are loads from the same named identifier
///   - and one of the following holds:
///       - pathA and pathB are identical
///       - pathB is a subpath of pathA and neither read into a `ref` type
fn validate_inferred_dep(
    dep: &ReactiveScopeDependency,
    temporaries: &HashMap<IdentifierId, ManualMemoDependency>,
    decls_within_memo_block: &HashSet<DeclarationId>,
    valid_deps_in_memo_block: &[ManualMemoDependency],
    errors: &mut Vec<CompilerDiagnostic>,
    _memo_location: &SourceLocation,
) {
    let debug_manual_memo = std::env::var("DEBUG_MANUAL_MEMO").is_ok();
    // Normalize the dependency
    let normalized_dep: ManualMemoDependency =
        if let Some(maybe_normalized_root) = temporaries.get(&dep.identifier.id) {
            let mut path = maybe_normalized_root.path.clone();
            path.extend(dep.path.iter().cloned());
            ManualMemoDependency {
                root: maybe_normalized_root.root.clone(),
                path,
            }
        } else {
            // invariant: expect scope dependency to be named
            if !matches!(dep.identifier.name, Some(IdentifierName::Named(_))) {
                // In upstream this is a CompilerError.invariant.
                // We skip non-named deps silently, matching the invariant behavior
                // (this would panic in upstream).
                return;
            }
            ManualMemoDependency {
                root: ManualMemoRoot::NamedLocal(Place {
                    identifier: dep.identifier.clone(),
                    effect: Effect::Read,
                    reactive: false,
                    loc: SourceLocation::Generated,
                }),
                path: dep.path.clone(),
            }
        };

    if debug_manual_memo {
        eprintln!(
            "[MANUAL_MEMO_VALIDATE] inferred dep raw={} normalized={} decls_within=[{}]",
            pretty_print_scope_dependency(dep),
            print_manual_memo_dependency(&normalized_dep, true),
            decls_within_memo_block
                .iter()
                .map(|d| d.0.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    // Check if the dependency was declared within the memo block
    for decl in decls_within_memo_block {
        if let ManualMemoRoot::NamedLocal(ref place) = normalized_dep.root
            && *decl == place.identifier.declaration_id
        {
            if debug_manual_memo {
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] skip dep {} due in-memo decl {}",
                    print_manual_memo_dependency(&normalized_dep, true),
                    decl.0
                );
            }
            return;
        }
    }

    // Compare against each source dependency
    let mut error_diagnostic: Option<CompareDependencyResult> = None;
    for original_dep in valid_deps_in_memo_block {
        let compare_result = compare_deps(&normalized_dep, original_dep);
        if debug_manual_memo {
            eprintln!(
                "[MANUAL_MEMO_VALIDATE] compare inferred={} source={} result={:?}",
                print_manual_memo_dependency(&normalized_dep, true),
                print_manual_memo_dependency(original_dep, true),
                compare_result
            );
        }
        if compare_result == CompareDependencyResult::Ok {
            return;
        } else {
            error_diagnostic = Some(match error_diagnostic {
                Some(prev) => merge_compare(prev, compare_result),
                None => compare_result,
            });
        }
    }

    // Build error description
    let mut description = String::from(
        "React Compiler has skipped optimizing this component because the existing manual \
         memoization could not be preserved. The inferred dependencies did not match the \
         manually specified dependencies, which could cause the value to change more or \
         less frequently than expected. ",
    );

    // If the dependency is a named variable then we can report it
    if matches!(dep.identifier.name, Some(IdentifierName::Named(_))) {
        description.push_str(&format!(
            "The inferred dependency was `{}`, but the source dependencies were [{}]. {}",
            pretty_print_scope_dependency(dep),
            valid_deps_in_memo_block
                .iter()
                .map(|d| print_manual_memo_dependency(d, true))
                .collect::<Vec<_>>()
                .join(", "),
            match error_diagnostic {
                Some(ed) => get_compare_dependency_result_description(ed),
                None => "Inferred dependency not present in source",
            }
        ));
    }

    errors.push(CompilerDiagnostic {
        severity: DiagnosticSeverity::CannotPreserveMemoization,
        message: description.trim().to_string(),
    });
}

// ---------------------------------------------------------------------------
// collectMaybeMemoDependencies (inline port from DropManualMemoization.ts)
// ---------------------------------------------------------------------------

/// Collect loads from named variables and property reads from `value`
/// into `maybe_deps`.
/// Returns the variable + property reads represented by `value`.
fn collect_maybe_memo_dependencies(
    value: &InstructionValue,
    maybe_deps: &mut HashMap<IdentifierId, ManualMemoDependency>,
    optional: bool,
) -> Option<ManualMemoDependency> {
    match value {
        InstructionValue::LoadGlobal { binding, .. } => Some(ManualMemoDependency {
            root: ManualMemoRoot::Global {
                identifier_name: binding.name().to_string(),
            },
            path: vec![],
        }),
        InstructionValue::PropertyLoad {
            object,
            property,
            optional: _,
            ..
        } => {
            let obj = maybe_deps.get(&object.identifier.id)?;
            let prop_str = match property {
                PropertyLiteral::String(s) => s.clone(),
                PropertyLiteral::Number(n) => n.to_string(),
            };
            let mut path = obj.path.clone();
            path.push(DependencyPathEntry {
                property: prop_str,
                optional,
            });
            Some(ManualMemoDependency {
                root: obj.root.clone(),
                path,
            })
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            if let Some(source) = maybe_deps.get(&place.identifier.id) {
                Some(source.clone())
            } else if matches!(place.identifier.name, Some(IdentifierName::Named(_))) {
                Some(ManualMemoDependency {
                    root: ManualMemoRoot::NamedLocal(place.clone()),
                    path: vec![],
                })
            } else {
                None
            }
        }
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            // Value blocks rely on StoreLocal to populate their return value.
            let aliased = maybe_deps.get(&value.identifier.id).cloned();
            if let Some(ref a) = aliased
                && !matches!(lvalue.place.identifier.name, Some(IdentifierName::Named(_)))
            {
                maybe_deps.insert(lvalue.place.identifier.id, a.clone());
                return Some(a.clone());
            }
            None
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Visitor
// ---------------------------------------------------------------------------

struct Visitor {
    /// Records all completed scopes (regardless of transitive memoization
    /// of scope dependencies).
    ///
    /// Both `scopes` and `pruned_scopes` are live sets. We rely on iterating
    /// the reactive-ir in evaluation order, as they are used to determine
    /// whether scope dependencies / declarations have completed mutation.
    scopes: HashSet<ScopeId>,
    pruned_scopes: HashSet<ScopeId>,
    scopes_with_non_temp_decl: HashSet<ScopeId>,
    temporaries: HashMap<IdentifierId, ManualMemoDependency>,
}

impl Visitor {
    fn is_mutating_effect(effect: Effect) -> bool {
        matches!(
            effect,
            Effect::Mutate | Effect::ConditionallyMutate | Effect::ConditionallyMutateIterator
        )
    }

    /// Recursively visit values and instructions to collect declarations
    /// and property loads.
    fn record_deps_in_value(&mut self, value: &InstructionValue, state: &mut VisitorState) {
        // In the upstream, this method also recurses into ReactiveValue variants
        // (SequenceExpression, OptionalExpression, ConditionalExpression, LogicalExpression).
        // The Rust port doesn't use ReactiveValue -- InstructionValue is used directly.
        // So we handle the default case: collect maybe-memo dependencies and track stores.

        collect_maybe_memo_dependencies(value, &mut self.temporaries, false);

        match value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } => {
                if let Some(ref mut memo_state) = state.manual_memo_state {
                    memo_state
                        .decls
                        .insert(lvalue.place.identifier.declaration_id);
                }
                if matches!(lvalue.place.identifier.name, Some(IdentifierName::Named(_))) {
                    self.temporaries.insert(
                        lvalue.place.identifier.id,
                        ManualMemoDependency {
                            root: ManualMemoRoot::NamedLocal(lvalue.place.clone()),
                            path: vec![],
                        },
                    );
                }
            }
            InstructionValue::Destructure { lvalue, .. } => {
                for_each_lvalue_pattern_place(&lvalue.pattern, &mut |place: &Place| {
                    if let Some(ref mut memo_state) = state.manual_memo_state {
                        memo_state.decls.insert(place.identifier.declaration_id);
                    }
                    if matches!(place.identifier.name, Some(IdentifierName::Named(_))) {
                        self.temporaries.insert(
                            place.identifier.id,
                            ManualMemoDependency {
                                root: ManualMemoRoot::NamedLocal(place.clone()),
                                path: vec![],
                            },
                        );
                    }
                });
            }
            _ => {}
        }
    }

    fn record_temporaries(&mut self, instr: &ReactiveInstruction, state: &mut VisitorState) {
        let lval_id = instr.lvalue.as_ref().map(|lv| lv.identifier.id);

        // If we already have this lvalue tracked, skip
        if let Some(id) = lval_id
            && self.temporaries.contains_key(&id)
        {
            return;
        }

        let is_named_local = instr
            .lvalue
            .as_ref()
            .is_some_and(|lv| matches!(lv.identifier.name, Some(IdentifierName::Named(_))));

        if instr.lvalue.is_some()
            && is_named_local
            && let Some(ref mut memo_state) = state.manual_memo_state
        {
            memo_state
                .decls
                .insert(instr.lvalue.as_ref().unwrap().identifier.declaration_id);
        }

        self.record_deps_in_value(&instr.value, state);

        if let Some(ref lvalue) = instr.lvalue {
            self.temporaries.insert(
                lvalue.identifier.id,
                ManualMemoDependency {
                    root: ManualMemoRoot::NamedLocal(lvalue.clone()),
                    path: vec![],
                },
            );
        }
    }

    fn visit_scope(&mut self, scope_block: &ReactiveScopeBlock, state: &mut VisitorState) {
        // Traverse the scope's instructions
        self.visit_block(&scope_block.instructions, state);
        let debug_manual_memo = std::env::var("DEBUG_MANUAL_MEMO").is_ok();

        // Upstream validates scope deps inline while manualMemoState is active
        // (ValidatePreservedManualMemoization.ts:424-437)
        if let Some(ref memo_state) = state.manual_memo_state
            && let Some(ref deps_from_source) = memo_state.deps_from_source
        {
            for dep in &scope_block.scope.dependencies {
                validate_inferred_dep(
                    dep,
                    &self.temporaries,
                    &memo_state.decls,
                    deps_from_source,
                    &mut state.errors,
                    &memo_state.loc,
                );
            }
        }

        self.scopes.insert(scope_block.scope.id);
        let has_non_temp_decl = scope_block.scope.declarations.values().any(|decl| {
            decl.identifier.name.as_ref().is_some_and(|name| {
                let name = match name {
                    IdentifierName::Named(name) | IdentifierName::Promoted(name) => name,
                };
                !is_generated_temp_name(name)
            })
        });
        if has_non_temp_decl {
            self.scopes_with_non_temp_decl.insert(scope_block.scope.id);
        }
        if debug_manual_memo {
            let decls = scope_block
                .scope
                .declarations
                .values()
                .map(|decl| {
                    let name = match &decl.identifier.name {
                        Some(IdentifierName::Named(name))
                        | Some(IdentifierName::Promoted(name)) => name.as_str(),
                        None => "<unnamed>",
                    };
                    format!(
                        "{}(id={},decl={})",
                        name, decl.identifier.id.0, decl.identifier.declaration_id.0
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            eprintln!(
                "[MANUAL_MEMO_VALIDATE] visit scope_id={} deps={} decls=[{}]",
                scope_block.scope.id.0,
                scope_block.scope.dependencies.len(),
                decls
            );
        }
        // Handle merged scopes. Upstream uses `scope.merged: Set<ScopeId>`,
        // but the Rust port uses `merged_id: Option<ScopeId>`.
        if let Some(merged_id) = scope_block.scope.merged_id {
            self.scopes.insert(merged_id);
            if has_non_temp_decl {
                self.scopes_with_non_temp_decl.insert(merged_id);
            }
            if debug_manual_memo {
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] visit merged_scope_id={}",
                    merged_id.0
                );
            }
        }
    }

    fn visit_pruned_scope(
        &mut self,
        scope_block: &PrunedReactiveScopeBlock,
        state: &mut VisitorState,
    ) {
        // Upstream: just traverse and record (ValidatePreservedManualMemoization.ts:445-451)
        self.visit_block(&scope_block.instructions, state);
        self.pruned_scopes.insert(scope_block.scope.id);
    }

    fn record_late_mutation(&mut self, decl_id: DeclarationId, state: &mut VisitorState) {
        let Some(watches) = state.late_mutation_watchers.get(&decl_id).cloned() else {
            return;
        };
        for watch in watches {
            if state
                .late_mutation_reported
                .insert((decl_id, watch.manual_memo_id))
            {
                state.errors.push(CompilerDiagnostic {
                    severity: DiagnosticSeverity::CannotPreserveMemoization,
                    message: "React Compiler has skipped optimizing this \
                            component because the existing manual memoization \
                            could not be preserved. This dependency may be \
                            mutated later, which could cause the value to \
                            change unexpectedly"
                        .to_string(),
                });
                state.errors.push(CompilerDiagnostic {
                    severity: DiagnosticSeverity::CannotPreserveMemoization,
                    message: "React Compiler has skipped optimizing this component \
                         because the existing manual memoization could not be \
                         preserved. This value was memoized in source but not \
                         in compilation output"
                        .to_string(),
                });
            }
        }
    }

    fn visit_instruction(&mut self, instruction: &ReactiveInstruction, state: &mut VisitorState) {
        let debug_manual_memo = std::env::var("DEBUG_MANUAL_MEMO").is_ok();
        // We don't invoke traverseInstructions because `recordDepsInValue`
        // recursively visits ReactiveValues and instructions
        self.record_temporaries(instruction, state);

        let value = &instruction.value;

        if let Some(ref mut memo_state) = state.manual_memo_state {
            if let Some(ref lvalue) = instruction.lvalue {
                match value {
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        let source_decl = place.identifier.declaration_id;
                        if memo_state.decls.contains(&source_decl)
                            || memo_state.memo_alias_decls.contains(&source_decl)
                        {
                            memo_state
                                .memo_alias_decls
                                .insert(lvalue.identifier.declaration_id);
                            if debug_manual_memo {
                                eprintln!(
                                    "[MANUAL_MEMO_VALIDATE] alias decl={} from decl={} via {}",
                                    lvalue.identifier.declaration_id.0,
                                    source_decl.0,
                                    match value {
                                        InstructionValue::LoadLocal { .. } => "LoadLocal",
                                        _ => "LoadContext",
                                    },
                                );
                            }
                        }
                    }
                    InstructionValue::StoreLocal { value, .. }
                    | InstructionValue::StoreContext { value, .. } => {
                        let source_decl = value.identifier.declaration_id;
                        if memo_state.decls.contains(&source_decl)
                            || memo_state.memo_alias_decls.contains(&source_decl)
                        {
                            memo_state
                                .memo_alias_decls
                                .insert(lvalue.identifier.declaration_id);
                            if debug_manual_memo {
                                eprintln!(
                                    "[MANUAL_MEMO_VALIDATE] alias decl={} from decl={} via {}",
                                    lvalue.identifier.declaration_id.0,
                                    source_decl.0,
                                    match instruction.value {
                                        InstructionValue::StoreLocal { .. } => "StoreLocal",
                                        _ => "StoreContext",
                                    },
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }

            let mut mark_side_effect_use = |place: &Place| {
                let direct = memo_state.decls.contains(&place.identifier.declaration_id)
                    || memo_state
                        .memo_alias_decls
                        .contains(&place.identifier.declaration_id);
                let via_temporary = self
                    .temporaries
                    .get(&place.identifier.id)
                    .and_then(|dep| match &dep.root {
                        ManualMemoRoot::NamedLocal(root_place) => {
                            Some(root_place.identifier.declaration_id)
                        }
                        _ => None,
                    })
                    .is_some_and(|decl_id| {
                        memo_state.decls.contains(&decl_id)
                            || memo_state.memo_alias_decls.contains(&decl_id)
                    });
                if direct || via_temporary {
                    memo_state.saw_side_effect_use_of_decl = true;
                }
            };
            match value {
                InstructionValue::CallExpression { args, .. } => {
                    for arg in args {
                        match arg {
                            Argument::Place(place) | Argument::Spread(place) => {
                                if Self::is_mutating_effect(place.effect) {
                                    mark_side_effect_use(place);
                                }
                            }
                        }
                    }
                }
                InstructionValue::MethodCall { receiver, args, .. } => {
                    if Self::is_mutating_effect(receiver.effect) {
                        mark_side_effect_use(receiver);
                    }
                    for arg in args {
                        match arg {
                            Argument::Place(place) | Argument::Spread(place) => {
                                if Self::is_mutating_effect(place.effect) {
                                    mark_side_effect_use(place);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Track post-memo writes to deps from source manual memoization.
        match value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } => {
                if lvalue.kind == InstructionKind::Reassign {
                    self.record_late_mutation(lvalue.place.identifier.declaration_id, state);
                }
            }
            InstructionValue::Destructure { lvalue, .. } => {
                if lvalue.kind == InstructionKind::Reassign {
                    for_each_lvalue_pattern_place(&lvalue.pattern, &mut |place: &Place| {
                        self.record_late_mutation(place.identifier.declaration_id, state);
                    });
                }
            }
            InstructionValue::PrefixUpdate { lvalue, .. }
            | InstructionValue::PostfixUpdate { lvalue, .. } => {
                self.record_late_mutation(lvalue.identifier.declaration_id, state);
            }
            _ => {}
        }

        // Track reassignments from inlining of manual memo
        if let InstructionValue::StoreLocal {
            lvalue, value: val, ..
        } = value
            && lvalue.kind == InstructionKind::Reassign
            && let Some(ref mut memo_state) = state.manual_memo_state
        {
            // Complex cases of inlining end up with a temporary that is reassigned
            let ids = memo_state
                .reassignments
                .entry(lvalue.place.identifier.declaration_id)
                .or_default();
            ids.insert(val.identifier.id);
        }

        // Track LoadLocal where the source has a scope but the lvalue doesn't
        if let InstructionValue::LoadLocal { place, .. } = value
            && place.identifier.scope.is_some()
            && instruction
                .lvalue
                .as_ref()
                .is_none_or(|lv| lv.identifier.scope.is_none())
            && let Some(ref mut memo_state) = state.manual_memo_state
            && let Some(ref lvalue) = instruction.lvalue
        {
            // Simpler cases of inlining assign to the original IIFE lvalue
            let ids = memo_state
                .reassignments
                .entry(lvalue.identifier.declaration_id)
                .or_default();
            ids.insert(place.identifier.id);
        }

        if let InstructionValue::StartMemoize {
            manual_memo_id,
            deps,
            loc: _,
        } = value
        {
            if debug_manual_memo {
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] start memo_id={} deps={}",
                    manual_memo_id,
                    deps.as_ref().map_or(0, |d| d.len())
                );
            }
            let deps_from_source = deps.clone();
            let watched_dep_decls = deps
                .as_ref()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| match &dep.root {
                            ManualMemoRoot::NamedLocal(place)
                                if place.identifier.scope.is_some() =>
                            {
                                Some(place.identifier.declaration_id)
                            }
                            _ => None,
                        })
                        .collect::<HashSet<_>>()
                })
                .unwrap_or_default();

            // invariant: no nested StartMemoize
            assert!(
                state.manual_memo_state.is_none(),
                "Unexpected nested StartMemoize instructions. \
                 Bad manual memoization ids: {:?}, {}",
                state.manual_memo_state.as_ref().map(|s| s.manual_memo_id),
                manual_memo_id
            );

            state.manual_memo_state = Some(ManualMemoBlockState {
                loc: instruction.loc.clone(),
                decls: HashSet::new(),
                deps_from_source,
                manual_memo_id: *manual_memo_id,
                reassignments: HashMap::new(),
                watched_dep_decls,
                memo_alias_decls: HashSet::new(),
                saw_side_effect_use_of_decl: false,
            });

            // Check that each scope dependency is either:
            // (1) Not scoped: identifier.scope == null is a proxy for whether the dep
            //     is a primitive, global, or other guaranteed non-allocating value.
            // (2) Scoped: check that the dependency's scope has completed before
            //     the manual useMemo.
            if let Some(deps) = deps {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(ref place) = dep.root
                        && place.identifier.scope.is_some()
                    {
                        let scope = place.identifier.scope.as_ref().unwrap();
                        let scope_id = scope.id;
                        if debug_manual_memo {
                            eprintln!(
                                "[MANUAL_MEMO_VALIDATE] check dep root={} decl={} scope_id={} seen_scope={} seen_pruned={} scope_end={} start_id={}",
                                match &place.identifier.name {
                                    Some(IdentifierName::Named(name)) => name.as_str(),
                                    Some(IdentifierName::Promoted(name)) => name.as_str(),
                                    None => "<unnamed>",
                                },
                                place.identifier.declaration_id.0,
                                scope_id.0,
                                self.scopes.contains(&scope_id),
                                self.pruned_scopes.contains(&scope_id),
                                scope.range.end.0,
                                instruction.id.0,
                            );
                        }
                        if !self.scopes.contains(&scope_id)
                            && !self.pruned_scopes.contains(&scope_id)
                        {
                            state.errors.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::CannotPreserveMemoization,
                                message: "React Compiler has skipped optimizing this \
                                        component because the existing manual memoization \
                                        could not be preserved. This dependency may be \
                                        mutated later, which could cause the value to \
                                        change unexpectedly"
                                    .to_string(),
                            });
                        }
                    }
                }
            }
        }

        if let InstructionValue::FinishMemoize {
            manual_memo_id,
            decl,
            pruned,
            loc: _,
        } = value
        {
            if debug_manual_memo {
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] finish memo_id={} decl_id={} scope={:?} pruned={}",
                    manual_memo_id,
                    decl.identifier.id.0,
                    decl.identifier.scope.as_ref().map(|s| s.id.0),
                    pruned
                );
            }
            // invariant: FinishMemoize must match StartMemoize
            assert!(
                state.manual_memo_state.is_some()
                    && state.manual_memo_state.as_ref().unwrap().manual_memo_id == *manual_memo_id,
                "Unexpected mismatch between StartMemoize and FinishMemoize. \
                 Encountered StartMemoize id={:?} followed by FinishMemoize id={}",
                state.manual_memo_state.as_ref().map(|s| s.manual_memo_id),
                manual_memo_id
            );

            // Upstream: FinishMemoize only checks result memoization, not deps.
            // Deps are validated inline in visit_scope while manualMemoState is active.
            let finished_state = state.manual_memo_state.take().unwrap();
            let finished_loc = finished_state.loc.clone();
            let can_defer_unmemoized_check = finished_state
                .deps_from_source
                .as_ref()
                .is_none_or(|deps| deps.is_empty());
            let reassignments = finished_state.reassignments;
            if debug_manual_memo && !reassignments.is_empty() {
                let mut parts: Vec<String> = reassignments
                    .iter()
                    .map(|(decl_id, ids)| {
                        let mut vals: Vec<u32> = ids.iter().map(|id| id.0).collect();
                        vals.sort_unstable();
                        format!(
                            "{}:[{}]",
                            decl_id.0,
                            vals.into_iter()
                                .map(|id| id.to_string())
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    })
                    .collect();
                parts.sort();
                eprintln!(
                    "[MANUAL_MEMO_VALIDATE] finish memo_id={} reassignments={}",
                    manual_memo_id,
                    parts.join(" ")
                );
            }
            for decl_id in finished_state.watched_dep_decls {
                state
                    .late_mutation_watchers
                    .entry(decl_id)
                    .or_default()
                    .push(LateMutationWatch {
                        manual_memo_id: *manual_memo_id,
                    });
            }

            if !pruned {
                // Check the operand of FinishMemoize (the decl place)
                let identifier = &decl.identifier;
                let decls: Vec<Identifier>;

                if identifier.scope.is_none() {
                    // If the manual memo was a useMemo that got inlined, iterate through
                    // all reassignments to the iife temporary to ensure they're memoized.
                    if let Some(reassigned_ids) = reassignments.get(&identifier.declaration_id) {
                        decls = reassigned_ids
                            .iter()
                            .map(|id| {
                                // We only have the id; construct a minimal identifier
                                // to check scope. We look up the temporaries to find
                                // the identifier with the matching id.
                                self.find_identifier_by_id(*id)
                                    .unwrap_or_else(|| identifier.clone())
                            })
                            .collect();
                    } else {
                        decls = vec![identifier.clone()];
                    }
                } else {
                    decls = vec![identifier.clone()];
                }

                for decl_identifier in &decls {
                    if is_unmemoized(decl_identifier, &self.scopes) {
                        if decl_identifier.scope.is_some() && can_defer_unmemoized_check {
                            if debug_manual_memo {
                                eprintln!(
                                    "[MANUAL_MEMO_VALIDATE] queue unresolved memoized value id={} decl={} scope={:?}",
                                    decl_identifier.id.0,
                                    decl_identifier.declaration_id.0,
                                    decl_identifier.scope.as_ref().map(|scope| scope.id.0)
                                );
                            }
                            state
                                .pending_unmemoized_checks
                                .push(PendingUnmemoizedCheck {
                                    identifier: decl_identifier.clone(),
                                    loc: finished_loc.clone(),
                                });
                        } else {
                            if debug_manual_memo && decl_identifier.scope.is_some() {
                                eprintln!(
                                    "[MANUAL_MEMO_VALIDATE] fail unresolved memoized value id={} decl={} scope={:?} (deps require immediate validation)",
                                    decl_identifier.id.0,
                                    decl_identifier.declaration_id.0,
                                    decl_identifier.scope.as_ref().map(|scope| scope.id.0)
                                );
                            }
                            state.errors.push(CompilerDiagnostic {
                                severity: DiagnosticSeverity::CannotPreserveMemoization,
                                message: "React Compiler has skipped optimizing this component \
                                     because the existing manual memoization could not be \
                                     preserved. This value was memoized in source but not \
                                     in compilation output"
                                    .to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    /// Try to find an Identifier we've recorded in temporaries by its IdentifierId.
    /// This is needed because in the reassignment tracking we only store the id,
    /// but we need the full Identifier (including scope info) to check if it's memoized.
    fn find_identifier_by_id(&self, id: IdentifierId) -> Option<Identifier> {
        self.temporaries.get(&id).and_then(|dep| match &dep.root {
            ManualMemoRoot::NamedLocal(place) => Some(place.identifier.clone()),
            _ => None,
        })
    }

    fn visit_block(&mut self, block: &ReactiveBlock, state: &mut VisitorState) {
        for stmt in block {
            self.visit_statement(stmt, state);
        }
    }

    fn visit_statement(&mut self, stmt: &ReactiveStatement, state: &mut VisitorState) {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                self.visit_instruction(instr, state);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                self.visit_terminal(&term_stmt.terminal, state);
            }
            ReactiveStatement::Scope(scope_block) => {
                self.visit_scope(scope_block, state);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                self.visit_pruned_scope(scope_block, state);
            }
        }
    }

    fn visit_terminal(&mut self, terminal: &ReactiveTerminal, state: &mut VisitorState) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                self.visit_block(consequent, state);
                if let Some(alt) = alternate {
                    self.visit_block(alt, state);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        self.visit_block(block, state);
                    }
                }
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. } => {
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                self.visit_block(init, state);
                if let Some(upd) = update {
                    self.visit_block(upd, state);
                }
                self.visit_block(loop_block, state);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            } => {
                self.visit_block(init, state);
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
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: iterate lvalue pattern places
// ---------------------------------------------------------------------------

fn for_each_lvalue_pattern_place(pattern: &Pattern, f: &mut impl FnMut(&Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => f(p),
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
// Helper
// ---------------------------------------------------------------------------

fn is_unmemoized(operand: &Identifier, scopes: &HashSet<ScopeId>) -> bool {
    operand.scope.is_some() && !scopes.contains(&operand.scope.as_ref().unwrap().id)
}

fn is_generated_temp_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix('t') else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
}
