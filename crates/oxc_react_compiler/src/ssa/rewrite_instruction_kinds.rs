//! Rewrite Instruction Kinds Based On Reassignment
//!
//! Port of `RewriteInstructionKindsBasedOnReassignment.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Rewrites InstructionKind of StoreLocal/Destructure instructions:
//! - First declaration of a named variable → Const (if never reassigned)
//! - If subsequently reassigned → first becomes Let, subsequent become Reassign
//! - PrefixUpdate/PostfixUpdate → declaration becomes Let

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity, ErrorCategory};
use crate::hir::types::*;

/// Rewrite instruction kinds so that variables which are never reassigned
/// use `Const` and those that are use `Let` (first) / `Reassign` (subsequent).
pub fn rewrite_instruction_kinds(func: &mut HIRFunction) -> Result<(), CompilerError> {
    let debug_rewrite = std::env::var("DEBUG_REWRITE_KINDS").is_ok();
    // First pass: collect which declaration_ids are reassigned.
    let mut store_count: HashMap<DeclarationId, u32> = HashMap::new();
    let mut generated_undefined_ids: HashSet<IdentifierId> = HashSet::new();

    // Params are implicitly declarations
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        if place.identifier.name.is_some() {
            store_count.insert(place.identifier.declaration_id, 1);
        }
    }

    // Build a map from identifier id → declaration_id for named LoadLocals.
    // This lets us trace PostfixUpdate/PrefixUpdate operands (which are temporaries
    // from LoadLocal) back to the original named variable's declaration_id.
    let mut id_to_decl: HashMap<IdentifierId, DeclarationId> = HashMap::new();

    // Collect for-loop init/update blocks and all loop body blocks
    let mut for_loop_init_blocks: HashSet<BlockId> = HashSet::new();
    let mut for_loop_update_blocks: HashSet<BlockId> = HashSet::new();
    let mut loop_body_blocks: HashSet<BlockId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        match &block.terminal {
            Terminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                for_loop_init_blocks.insert(*init);
                if let Some(u) = update {
                    for_loop_update_blocks.insert(*u);
                }
                loop_body_blocks.insert(*loop_block);
            }
            Terminal::While { loop_block, .. } | Terminal::DoWhile { loop_block, .. } => {
                loop_body_blocks.insert(*loop_block);
            }
            Terminal::ForOf { loop_block, .. } | Terminal::ForIn { loop_block, .. } => {
                loop_body_blocks.insert(*loop_block);
            }
            _ => {}
        }
    }

    // Scan all instructions to:
    // 1. Count stores per declaration_id
    // 2. Build LoadLocal id→declaration_id map
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. } => {
                    if lvalue.place.identifier.name.is_some() {
                        let decl_id = lvalue.place.identifier.declaration_id;
                        *store_count.entry(decl_id).or_insert(0) += 1;
                    }
                }
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    ..
                } => {
                    generated_undefined_ids.insert(instr.lvalue.identifier.id);
                }
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    // DeclareLocal counts as a "store" for the purpose of determining
                    // if a variable is reassigned. If a DeclareLocal is followed by a
                    // StoreLocal for the same variable, that's 2 stores → Let + Reassign.
                    let decl_id = lvalue.place.identifier.declaration_id;
                    *store_count.entry(decl_id).or_insert(0) += 1;
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    for place in pattern_places(&lvalue.pattern) {
                        if place.identifier.name.is_some() {
                            let decl_id = place.identifier.declaration_id;
                            *store_count.entry(decl_id).or_insert(0) += 1;
                        }
                    }
                }
                InstructionValue::LoadLocal { place, .. } => {
                    // Map the instruction's lvalue (temp) id → the loaded variable's declaration_id
                    if place.identifier.name.is_some() {
                        id_to_decl
                            .insert(instr.lvalue.identifier.id, place.identifier.declaration_id);
                    }
                }
                InstructionValue::PrefixUpdate { value, .. }
                | InstructionValue::PostfixUpdate { value, .. } => {
                    // The value is a temp from LoadLocal. Trace back to the named variable.
                    if let Some(&decl_id) = id_to_decl.get(&value.identifier.id) {
                        let count = store_count.entry(decl_id).or_insert(0);
                        if *count < 2 {
                            *count = 2;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Determine which declaration_ids are reassigned (stored more than once)
    let reassigned: HashSet<DeclarationId> = store_count
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(id, _)| *id)
        .collect();

    // Collect declaration_ids that are modified in for-loop update blocks.
    let mut for_update_modified_decls: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        if for_loop_update_blocks.contains(&block.id) {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        if lvalue.place.identifier.name.is_some() {
                            for_update_modified_decls
                                .insert(lvalue.place.identifier.declaration_id);
                        }
                    }
                    InstructionValue::PrefixUpdate { value, .. }
                    | InstructionValue::PostfixUpdate { value, .. } => {
                        if let Some(&decl_id) = id_to_decl.get(&value.identifier.id) {
                            for_update_modified_decls.insert(decl_id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Collect declaration_ids REASSIGNED in any loop body block (while, do-while, for-of, for-in, for).
    // Only track Reassign stores (not initial Let/Const declarations), because variables
    // declared fresh inside a loop body should remain const if never reassigned.
    let mut loop_body_modified_decls: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        if loop_body_blocks.contains(&block.id) {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        if lvalue.kind == InstructionKind::Reassign
                            && lvalue.place.identifier.name.is_some()
                        {
                            loop_body_modified_decls.insert(lvalue.place.identifier.declaration_id);
                        }
                    }
                    InstructionValue::PrefixUpdate { value, .. }
                    | InstructionValue::PostfixUpdate { value, .. } => {
                        if let Some(&decl_id) = id_to_decl.get(&value.identifier.id) {
                            loop_body_modified_decls.insert(decl_id);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // For-init variables that are also modified in the update block should not be promoted
    let mut for_init_decl_ids: HashSet<DeclarationId> = HashSet::new();
    for (_, block) in &func.body.blocks {
        if for_loop_init_blocks.contains(&block.id) {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && lvalue.place.identifier.name.is_some()
                {
                    let decl_id = lvalue.place.identifier.declaration_id;
                    if for_update_modified_decls.contains(&decl_id)
                        || loop_body_modified_decls.contains(&decl_id)
                    {
                        for_init_decl_ids.insert(decl_id);
                    }
                }
            }
        }
    }

    // Track declarations in traversal order for destructure-kind parity with upstream:
    // Destructure is Const when introducing identifiers, Reassign when targeting existing ones.
    let mut seen_decls_in_order: HashSet<DeclarationId> = HashSet::new();
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        if place.identifier.name.is_some() {
            seen_decls_in_order.insert(place.identifier.declaration_id);
        }
    }
    for place in &func.context {
        if place.identifier.name.is_some() {
            seen_decls_in_order.insert(place.identifier.declaration_id);
        }
    }

    // Second pass: rewrite kinds using the ORIGINAL InstructionKind to determine
    // whether this is a declaration or a reassignment (not iteration order).
    for (_, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if lvalue.place.identifier.name.is_some() {
                        seen_decls_in_order.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    if lvalue.place.identifier.name.is_some() {
                        if debug_rewrite {
                            eprintln!(
                                "[DEBUG_REWRITE_KINDS] storelocal decl={} name={:?} kind_before={:?} value_name={:?} value_loc={:?}",
                                lvalue.place.identifier.declaration_id.0,
                                lvalue.place.identifier.name,
                                lvalue.kind,
                                value.identifier.name,
                                value.identifier.loc
                            );
                        }
                        let decl_id = lvalue.place.identifier.declaration_id;
                        let is_reassigned = reassigned.contains(&decl_id);
                        // Don't promote for-loop init variables or variables
                        // modified inside any loop body
                        let in_for_init = for_init_decl_ids.contains(&decl_id);
                        let in_loop_body = loop_body_modified_decls.contains(&decl_id);
                        let is_placeholder_undefined =
                            is_placeholder_undefined_value_for_let(value, &generated_undefined_ids);
                        match lvalue.kind {
                            InstructionKind::Let
                            | InstructionKind::Const
                            | InstructionKind::HoistedConst
                            | InstructionKind::Function
                            | InstructionKind::HoistedFunction
                            | InstructionKind::Catch => {
                                // Catch is included here: StoreLocal{Catch} gets the same
                                // const/let promotion as other declarations. The DeclareLocal{Catch}
                                // for the catch parameter itself is NOT a StoreLocal, so it's
                                // unaffected by this pass and keeps its Catch kind.
                                if is_placeholder_undefined
                                    || is_reassigned
                                    || in_for_init
                                    || in_loop_body
                                {
                                    lvalue.kind = InstructionKind::Let;
                                } else {
                                    lvalue.kind = InstructionKind::Const;
                                }
                            }
                            InstructionKind::HoistedLet => {
                                // Hoisted let declarations stay as let — they were originally
                                // declared with `let` in the source and hoisting doesn't change
                                // that. The upstream compiler preserves this via DeclareLocal.
                                lvalue.kind = InstructionKind::Let;
                            }
                            InstructionKind::Reassign => {
                                // Keep as Reassign
                            }
                        }
                        seen_decls_in_order.insert(decl_id);
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    if debug_rewrite {
                        eprintln!(
                            "[DEBUG_REWRITE_KINDS] destructure start kind={:?} pattern_places={}",
                            lvalue.kind,
                            pattern_places(&lvalue.pattern).len()
                        );
                    }
                    let mut first_reassign_place: Option<IdentifierId> = None;
                    let mut first_declaration_place: Option<IdentifierId> = None;
                    let mut declaration_requires_let = false;
                    for place in pattern_places(&lvalue.pattern) {
                        let is_reassign = if place.identifier.name.is_some() {
                            let decl_id = place.identifier.declaration_id;
                            if seen_decls_in_order.contains(&decl_id) {
                                true
                            } else {
                                if block.kind == BlockKind::Value {
                                    return Err(CompilerError::Bail(BailOut {
                                        reason: "TODO: Handle reassignment in a value block where the original declaration was removed by dead code elimination (DCE)".to_string(),
                                        diagnostics: vec![CompilerDiagnostic {
                                            severity: DiagnosticSeverity::Invariant,
                                            message: format!(
                                                "No declaration for destructured identifier {} in value block",
                                                place.identifier.id
                                            ),
                                            category: Some(ErrorCategory::Invariant),
                                        }],
                                    }));
                                }
                                seen_decls_in_order.insert(decl_id);
                                if reassigned.contains(&decl_id) {
                                    declaration_requires_let = true;
                                }
                                false
                            }
                        } else {
                            false
                        };
                        if debug_rewrite {
                            eprintln!(
                                "[DEBUG_REWRITE_KINDS]   place id={} name={} decl={} computed_kind={:?} reassigned={}",
                                place.identifier.id,
                                place.identifier.name.is_some(),
                                place.identifier.declaration_id.0,
                                if is_reassign {
                                    InstructionKind::Reassign
                                } else if declaration_requires_let {
                                    InstructionKind::Let
                                } else {
                                    InstructionKind::Const
                                },
                                reassigned.contains(&place.identifier.declaration_id)
                            );
                        }
                        if is_reassign {
                            if let Some(prev_decl_id) = first_declaration_place {
                                if debug_rewrite {
                                    eprintln!(
                                        "[DEBUG_REWRITE_KINDS]   mixed destructure kind prev={:?} current={:?}",
                                        InstructionKind::Const,
                                        InstructionKind::Reassign
                                    );
                                }
                                return Err(CompilerError::Bail(BailOut {
                                    reason: "Expected consistent kind for destructuring"
                                        .to_string(),
                                    diagnostics: vec![CompilerDiagnostic {
                                        severity: DiagnosticSeverity::Invariant,
                                        message: format!(
                                            "Other places were `Const` but identifier {} is Reassign (declaration place was {})",
                                            place.identifier.id, prev_decl_id
                                        ),
                                        category: Some(ErrorCategory::Invariant),
                                    }],
                                }));
                            }
                            first_reassign_place.get_or_insert(place.identifier.id);
                        } else {
                            if let Some(prev_reassign_id) = first_reassign_place {
                                if debug_rewrite {
                                    eprintln!(
                                        "[DEBUG_REWRITE_KINDS]   mixed destructure kind prev={:?} current={:?}",
                                        InstructionKind::Reassign,
                                        if declaration_requires_let {
                                            InstructionKind::Let
                                        } else {
                                            InstructionKind::Const
                                        }
                                    );
                                }
                                return Err(CompilerError::Bail(BailOut {
                                    reason: "Expected consistent kind for destructuring"
                                        .to_string(),
                                    diagnostics: vec![CompilerDiagnostic {
                                        severity: DiagnosticSeverity::Invariant,
                                        message: format!(
                                            "Other places were `Reassign` but identifier {} is Const/Let (reassign place was {})",
                                            place.identifier.id, prev_reassign_id
                                        ),
                                        category: Some(ErrorCategory::Invariant),
                                    }],
                                }));
                            }
                            first_declaration_place.get_or_insert(place.identifier.id);
                        }
                    }
                    if first_reassign_place.is_some() {
                        lvalue.kind = InstructionKind::Reassign;
                    } else if first_declaration_place.is_some() {
                        lvalue.kind = if declaration_requires_let {
                            InstructionKind::Let
                        } else {
                            InstructionKind::Const
                        };
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn is_placeholder_undefined_value_for_let(
    value: &Place,
    generated_undefined_ids: &HashSet<IdentifierId>,
) -> bool {
    value
        .identifier
        .name
        .as_ref()
        .is_some_and(|name| match name {
            IdentifierName::Named(name) | IdentifierName::Promoted(name) => name == "undefined",
        })
        || generated_undefined_ids.contains(&value.identifier.id)
}

/// Extract all Place references from a destructuring pattern.
fn pattern_places(pattern: &Pattern) -> Vec<&Place> {
    let mut places = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => places.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => places.push(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => places.push(p),
                }
            }
        }
    }
    places
}
