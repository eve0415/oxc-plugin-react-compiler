//! Validates that all known effect dependencies are memoized.
//!
//! Port of `ValidateMemoizedEffectDependencies.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! The algorithm checks two things:
//! - Disallow effect dependencies that should be memoized (have a reactive scope assigned) but
//!   where that reactive scope does not exist. This checks for cases where a reactive scope was
//!   pruned for some reason, such as spanning a hook.
//! - Disallow effect dependencies whose mutable range encompasses the effect call.
//!
//! This pass runs on the **ReactiveFunction** (tree form), NOT the HIR.
//! It is gated behind `env.config.validateMemoizedEffectDependencies`.

use std::collections::HashSet;

use crate::error::{
    BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory, extract_span,
};
use crate::hir::types::*;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Validates that all known effect dependencies are memoized.
///
/// Returns `Ok(())` if validation passes, or `Err(CompilerError)` with collected diagnostics.
pub fn validate_memoized_effect_dependencies(func: &ReactiveFunction) -> Result<(), CompilerError> {
    let mut state = VisitorState {
        scopes: HashSet::new(),
        diagnostics: Vec::new(),
    };
    visit_block(&func.body, &mut state);

    if state.diagnostics.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Unmemoized effect dependencies".to_string(),
            diagnostics: state.diagnostics,
        }))
    }
}

struct VisitorState {
    /// Set of scope IDs that exist in the reactive function tree.
    scopes: HashSet<ScopeId>,
    diagnostics: Vec<CompilerDiagnostic>,
}

// ---------------------------------------------------------------------------
// Recursive visitor
// ---------------------------------------------------------------------------

fn visit_block(block: &ReactiveBlock, state: &mut VisitorState) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                visit_instruction(instr, state);
            }
            ReactiveStatement::Scope(scope_block) => {
                visit_scope(scope_block, state);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                visit_block(&pruned.instructions, state);
            }
            ReactiveStatement::Terminal(term) => {
                visit_terminal(&term.terminal, state);
            }
        }
    }
}

fn visit_scope(scope_block: &ReactiveScopeBlock, state: &mut VisitorState) {
    // First traverse children
    visit_block(&scope_block.instructions, state);

    // Record scopes that exist in the AST so we can later check to see if
    // effect dependencies which should be memoized (have a scope assigned)
    // actually are memoized (that scope exists).
    // However, we only record scopes if *their* dependencies are also
    // memoized, allowing a transitive memoization check.
    let are_dependencies_memoized = scope_block
        .scope
        .dependencies
        .iter()
        .all(|dep| !is_unmemoized(&dep.identifier, &state.scopes));

    if are_dependencies_memoized {
        state.scopes.insert(scope_block.scope.id);
        if let Some(merged_id) = scope_block.scope.merged_id {
            state.scopes.insert(merged_id);
        }
        // Upstream tracks all merged scope ids in `scope.merged: Set<ScopeId>`.
        // Our IR only has `merged_id: Option<ScopeId>` and some merge passes
        // do not populate it, so recover equivalent aliases from identifiers
        // retained in merged declarations/dependencies.
        for decl in scope_block.scope.declarations.values() {
            if let Some(scope) = &decl.identifier.scope {
                state.scopes.insert(scope.id);
            }
        }
        for dep in &scope_block.scope.dependencies {
            if let Some(scope) = &dep.identifier.scope {
                state.scopes.insert(scope.id);
            }
        }
    }
}

fn visit_instruction(instr: &ReactiveInstruction, state: &mut VisitorState) {
    // Check for effect hook calls: callee(fn, deps, ...)
    if let InstructionValue::CallExpression { callee, args, .. } = &instr.value
        && is_effect_hook(&callee.identifier)
        && args.len() >= 2
        && let Argument::Place(deps_place) = &args[1]
    {
        let debug = std::env::var("DEBUG_MEMO_EFFECTS").ok().as_deref() == Some("1");
        // Check if the dependency array is mutable at this instruction
        // or if its scope was pruned (unmemoized)
        let mutable = is_mutable_at(instr.id, deps_place);
        let unmemoized = is_unmemoized(&deps_place.identifier, &state.scopes);
        if debug {
            eprintln!(
                "[MEMO_EFFECTS] effect-call id={} deps={} deps_scope={:?} range=[{}, {}) mutable={} unmemoized={} known_scopes={:?}",
                instr.id.0,
                deps_place
                    .identifier
                    .name
                    .as_ref()
                    .map(|n| n.value())
                    .unwrap_or("<temp>"),
                deps_place.identifier.scope.as_ref().map(|s| s.id.0),
                deps_place.identifier.mutable_range.start.0,
                deps_place.identifier.mutable_range.end.0,
                mutable,
                unmemoized,
                state.scopes.iter().map(|s| s.0).collect::<Vec<_>>(),
            );
        }
        if mutable || unmemoized {
            state.diagnostics.push(CompilerDiagnostic {
                        severity: DiagnosticSeverity::InvalidReact,
                        message: "React Compiler has skipped optimizing this component because the effect dependencies could not be memoized. Unmemoized effect dependencies can trigger an infinite loop or other unexpected behavior".to_string(),
                        category: ErrorCategory::EffectDependencies,
                        span: extract_span(&deps_place.loc),
                        ..Default::default()
                    });
        }
    }
}

fn visit_terminal(terminal: &ReactiveTerminal, state: &mut VisitorState) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            visit_block(consequent, state);
            if let Some(alt) = alternate {
                visit_block(alt, state);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    visit_block(block, state);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            visit_block(init, state);
            if let Some(update) = update {
                visit_block(update, state);
            }
            visit_block(loop_block, state);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            visit_block(init, state);
            visit_block(loop_block, state);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. } => {
            visit_block(loop_block, state);
        }
        ReactiveTerminal::Label { block, .. } => {
            visit_block(block, state);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            visit_block(block, state);
            visit_block(handler, state);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Checks if an identifier has a scope assigned but that scope doesn't exist
/// in the reactive function tree (i.e., the scope was pruned).
fn is_unmemoized(identifier: &Identifier, scopes: &HashSet<ScopeId>) -> bool {
    if let Some(ref scope) = identifier.scope {
        !scopes.contains(&scope.id)
    } else {
        false
    }
}

/// Checks if a place is mutable at the given instruction.
/// Equivalent to upstream `isMutable(instruction, place)`.
fn is_mutable_at(instr_id: InstructionId, place: &Place) -> bool {
    let range = &place.identifier.mutable_range;
    instr_id >= range.start && instr_id < range.end
}

/// Checks if an identifier is an effect hook.
/// Upstream includes `useEffect`, `useLayoutEffect`, and `useInsertionEffect`.
fn is_effect_hook(identifier: &Identifier) -> bool {
    matches!(
        &identifier.type_,
        Type::Function { shape_id: Some(shape_id), .. }
            if shape_id == "BuiltInUseEffectHookId"
                || shape_id == "BuiltInUseLayoutEffectHookId"
                || shape_id == "BuiltInUseInsertionEffectHookId"
    )
}
