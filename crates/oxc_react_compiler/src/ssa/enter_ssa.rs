//! EnterSSA — convert HIR to SSA form.
//!
//! Port of `EnterSSA.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Uses a simplified version of the Braun et al. algorithm.
//! Blocks are visited in order; each definition creates a new SSA identifier.

use std::collections::{HashMap, HashSet};

use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::builder::each_terminal_successor;
use crate::hir::types::*;
use crate::hir::visitors;

fn enter_ssa_successors(terminal: &Terminal) -> Vec<BlockId> {
    each_terminal_successor(terminal)
}

/// Convert an HIRFunction to SSA form in-place.
///
/// Each definition (lvalue) creates a fresh SSA identifier.
/// Each use (operand) is rewritten to the most recent definition.
/// Phi nodes are placed at merge points where needed.
pub fn enter_ssa(func: &mut HIRFunction) -> Result<(), CompilerError> {
    let mut next_id = 0u32;
    max_identifier_id_recursive(func, &mut next_id);
    enter_ssa_with_next_id(func, &mut next_id)
}

fn enter_ssa_with_next_id(func: &mut HIRFunction, next_id: &mut u32) -> Result<(), CompilerError> {
    let mut ctx = SSAContext::new(func, *next_id);

    // Define parameters at the entry block
    let entry = func.body.entry;
    for param in &mut func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => {
                let new_id = ctx.define(&p.identifier, entry, false);
                p.identifier = new_id;
            }
        }
    }

    // Upstream EnterSSA processes blocks in their existing CFG storage order.
    // BFS can visit Optional/Ternary fallthrough blocks before branch defs and
    // spuriously create use-before-def bailouts.
    for i in 0..func.body.blocks.len() {
        let block_id = func.body.blocks[i].0;
        ctx.current_block = block_id;
        ctx.visited.insert(block_id);

        let block = &mut func.body.blocks[i].1;

        // Process instructions
        for instr in &mut block.instructions {
            // Rewrite operands (reads) first
            visitors::map_instruction_operands(instr, |place| {
                if let Some(def) = ctx.lookup(place.identifier.id, block_id) {
                    place.identifier = def;
                } else {
                    ctx.mark_unknown_use(&place.identifier);
                }
            });
            if let Some(err) = ctx.take_pending_error() {
                return Err(err);
            }

            // Then rewrite lvalues (writes)
            let is_hoisted_function_declaration = matches!(
                &instr.value,
                InstructionValue::FunctionExpression {
                    expr_type: FunctionExpressionType::FunctionDeclaration,
                    ..
                }
            ) || matches!(
                &instr.value,
                InstructionValue::StoreLocal { lvalue, .. }
                    if matches!(
                        lvalue.kind,
                        InstructionKind::Function | InstructionKind::HoistedFunction
                    )
            );
            if std::env::var("DEBUG_SSA_HOIST").is_ok() {
                eprintln!(
                    "[SSA_HOIST] block={} instr#{} kind={:?} allow_undefined_use={}",
                    block_id.0, instr.id.0, instr.value, is_hoisted_function_declaration
                );
            }
            visitors::map_instruction_lvalues(instr, |place| {
                let new_id =
                    ctx.define(&place.identifier, block_id, is_hoisted_function_declaration);
                place.identifier = new_id;
            });
            if let Some(err) = ctx.take_pending_error() {
                return Err(err);
            }
        }

        // Rewrite terminal operands
        visitors::map_terminal_operands(&mut block.terminal, |place| {
            if let Some(def) = ctx.lookup(place.identifier.id, block_id) {
                place.identifier = def;
            } else {
                ctx.mark_unknown_use(&place.identifier);
            }
        });
        if let Some(err) = ctx.take_pending_error() {
            return Err(err);
        }

        // Track successors for phi placement
        let successors = enter_ssa_successors(&block.terminal);
        for succ_id in &successors {
            let count = ctx
                .unsealed
                .entry(*succ_id)
                .or_insert_with(|| ctx.pred_count.get(succ_id).copied().unwrap_or(0));
            if *count > 0 {
                *count -= 1;
            }
        }
    }

    // Upstream EnterSSA uses the predecessor sets already recorded on blocks.
    // Preserve that CFG shape instead of reconstructing it from the reduced
    // visitor successor set, which omits loop backedges/fallthrough structure.
    for (block_id, block) in &func.body.blocks {
        let mut preds: Vec<BlockId> = block.preds.iter().copied().collect();
        preds.sort_by_key(|id| id.0);
        ctx.pred_lists.insert(*block_id, preds);
    }

    for (block_id, _) in &func.body.blocks {
        let count = ctx.pred_lists.get(block_id).map_or(0, Vec::len);
        ctx.pred_count.insert(*block_id, count);
    }

    // Place phi nodes at merge points.
    // A single sweep is not enough for loop-carried values because one merge can
    // depend on a phi inserted at another merge later in the traversal. Iterate
    // to a fixpoint so loop headers and exits converge like upstream's sealed SSA.
    let mut phi_rewrites_by_block: HashMap<BlockId, HashMap<IdentifierId, Identifier>> =
        HashMap::new();
    let mut placed_phis: HashSet<(BlockId, IdentifierId)> = HashSet::new();
    let mut made_progress = true;
    while made_progress {
        made_progress = false;
        for i in 0..func.body.blocks.len() {
            let block_id = func.body.blocks[i].0;
            let preds: Vec<BlockId> = ctx.pred_lists.get(&block_id).cloned().unwrap_or_default();
            if preds.len() <= 1 {
                continue;
            }

            let mut all_orig_ids: Vec<IdentifierId> = ctx.global_defs.keys().copied().collect();
            all_orig_ids.sort_by_key(|id| id.0);

            let mut all_defs: HashMap<IdentifierId, Vec<(BlockId, Identifier)>> = HashMap::new();
            for &pred_id in &preds {
                for &orig_id in &all_orig_ids {
                    let ssa_id =
                        find_reaching_def(pred_id, orig_id, &ctx.block_defs, &ctx.pred_lists, func);
                    if let Some(ssa_id) = ssa_id {
                        all_defs.entry(orig_id).or_default().push((pred_id, ssa_id));
                    }
                }
            }

            let mut all_defs_sorted: Vec<(IdentifierId, Vec<(BlockId, Identifier)>)> =
                all_defs.into_iter().collect();
            all_defs_sorted.sort_by_key(|(orig_id, _)| orig_id.0);

            for (orig_id, pred_defs) in all_defs_sorted {
                if pred_defs.len() < 2 || placed_phis.contains(&(block_id, orig_id)) {
                    continue;
                }
                let first = &pred_defs[0].1.id;
                if pred_defs.iter().all(|(_, d)| d.id == *first) {
                    continue;
                }

                let phi_id = ctx.make_ssa_id(&pred_defs[0].1);
                if let Some(old_ssa) = ctx.global_defs.get(&orig_id) {
                    phi_rewrites_by_block
                        .entry(block_id)
                        .or_default()
                        .insert(old_ssa.id, phi_id.clone());
                }

                let mut operands = HashMap::new();
                for (pred_bid, ssa_id) in pred_defs {
                    let loc = ssa_id.loc.clone();
                    operands.insert(
                        pred_bid,
                        Place {
                            identifier: ssa_id,
                            effect: Effect::Unknown,
                            reactive: false,
                            loc,
                        },
                    );
                }
                let phi = Phi {
                    place: Place {
                        identifier: phi_id.clone(),
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: phi_id.loc.clone(),
                    },
                    operands,
                };

                func.body.blocks[i].1.phis.push(phi);
                ctx.set_phi_def(block_id, orig_id, phi_id);
                placed_phis.insert((block_id, orig_id));
                made_progress = true;
            }
        }
    }

    // Second pass: rewrite operands in blocks that should use phi outputs.
    // After phi placement, instructions in merge blocks and blocks downstream
    // still reference pre-phi SSA IDs. We must update them.
    //
    // IMPORTANT: Do NOT rewrite predecessor blocks of merge blocks — their
    // references to pre-phi SSA IDs are correct (they are the definitions that
    // FEED INTO the phi, not read from it).
    if !phi_rewrites_by_block.is_empty() {
        // For each block, compute per-block rewrite maps using the
        // now-correct block_defs (which includes phi outputs).
        // The old approach used a global rewrite map keyed by the
        // "current" global_defs entry, but that entry reflects the
        // LAST definition in BFS order, not what each merge block
        // actually referenced during pass 1. This caused cascading
        // phis (e.g. two consecutive if-without-else) to not rewrite
        // operands in intermediate merge blocks.
        for i in 0..func.body.blocks.len() {
            let block_id = func.body.blocks[i].0;

            // Compute the correct reaching def for each variable
            let mut all_orig_ids2: Vec<IdentifierId> = ctx.global_defs.keys().copied().collect();
            all_orig_ids2.sort_by_key(|id| id.0);
            let mut rewrites: HashMap<IdentifierId, Identifier> = HashMap::new();
            for orig_id in all_orig_ids2 {
                let correct = if let Some(bm) = ctx.block_defs.get(&block_id) {
                    if let Some(def) = bm.get(&orig_id) {
                        Some(def.clone())
                    } else {
                        find_reaching_def(block_id, orig_id, &ctx.block_defs, &ctx.pred_lists, func)
                    }
                } else {
                    find_reaching_def(block_id, orig_id, &ctx.block_defs, &ctx.pred_lists, func)
                };
                if let Some(correct_def) = correct {
                    for bm in ctx.block_defs.values() {
                        if let Some(ssa_id) = bm.get(&orig_id)
                            && ssa_id.id != correct_def.id
                        {
                            rewrites.insert(ssa_id.id, correct_def.clone());
                        }
                    }
                }
            }

            if rewrites.is_empty() {
                continue;
            }

            let block = &mut func.body.blocks[i].1;

            // Rewrite instruction operands (not lvalues or phi operands)
            for instr in &mut block.instructions {
                visitors::map_instruction_operands(instr, |place| {
                    if let Some(c) = rewrites.get(&place.identifier.id) {
                        place.identifier = c.clone();
                    }
                });
            }

            // Rewrite terminal operands
            visitors::map_terminal_operands(&mut block.terminal, |place| {
                if let Some(c) = rewrites.get(&place.identifier.id) {
                    place.identifier = c.clone();
                }
            });
        }
    }

    // Process nested functions only after the parent function has placed/re-written
    // phis so captured context places see the final SSA ids from this scope.
    for i in 0..func.body.blocks.len() {
        let block_id = func.body.blocks[i].0;
        let block = &mut func.body.blocks[i].1;
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    seed_nested_context(&mut lowered_func.func, &ctx, block_id);
                    enter_ssa_with_next_id(&mut lowered_func.func, &mut ctx.next_id)?;
                }
                _ => {}
            }
        }
    }
    *next_id = ctx.next_id;
    Ok(())
}

fn seed_nested_context(func: &mut HIRFunction, parent_ctx: &SSAContext, parent_block_id: BlockId) {
    // Build mapping from pre-SSA context IDs to parent SSA-versioned IDs
    let mut context_rewrites: HashMap<IdentifierId, Identifier> = HashMap::new();
    for place in &mut func.context {
        let old_id = place.identifier.id;
        if let Some(def) = parent_ctx.lookup(old_id, parent_block_id) {
            place.identifier = def.clone();
            context_rewrites.insert(old_id, def);
        }
    }

    // Update LoadContext instruction operands to match the updated func.context IDs.
    // Without this, LoadContext operands retain pre-SSA IDs while func.context has
    // SSA-versioned IDs, breaking downstream passes that compare them (e.g.,
    // collect_temporaries_impl's context variable guard in PropagateScopeDependenciesHIR).
    if !context_rewrites.is_empty() {
        for (_, block) in &mut func.body.blocks {
            for instr in &mut block.instructions {
                if let InstructionValue::LoadContext { place, .. } = &mut instr.value
                    && let Some(new_id) = context_rewrites.get(&place.identifier.id)
                {
                    place.identifier = new_id.clone();
                }
            }
        }
    }
}

fn max_identifier_id_recursive(func: &HIRFunction, max: &mut u32) {
    for param in &func.params {
        match param {
            Argument::Place(p) | Argument::Spread(p) => bump_max(max, p.identifier.id),
        }
    }
    for place in &func.context {
        bump_max(max, place.identifier.id);
    }
    bump_max(max, func.returns.identifier.id);

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            bump_max(max, phi.place.identifier.id);
            for op in phi.operands.values() {
                bump_max(max, op.identifier.id);
            }
        }
        for instr in &block.instructions {
            bump_max(max, instr.lvalue.identifier.id);
            visitors::for_each_instruction_operand(instr, |place| {
                bump_max(max, place.identifier.id);
            });
            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    max_identifier_id_recursive(&lowered_func.func, max);
                }
                _ => {}
            }
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            bump_max(max, place.identifier.id);
        });
    }
}

#[inline]
fn bump_max(max: &mut u32, id: IdentifierId) {
    *max = (*max).max(id.0 + 1);
}

/// Walk up the predecessor chain from `start_block` to find the reaching
/// definition for `orig_id`. At single-predecessor blocks, follow the
/// predecessor chain. At merge points, check if a phi was placed; if not,
/// recursively search through any predecessor (they should all agree if no
/// phi is needed). At the entry block (0 preds), stop.
fn find_reaching_def(
    start_block: BlockId,
    orig_id: IdentifierId,
    block_defs: &HashMap<BlockId, HashMap<IdentifierId, Identifier>>,
    pred_lists: &HashMap<BlockId, Vec<BlockId>>,
    func: &HIRFunction,
) -> Option<Identifier> {
    let mut memo: HashMap<(BlockId, IdentifierId), Option<Identifier>> = HashMap::new();

    fn helper(
        current: BlockId,
        orig_id: IdentifierId,
        block_defs: &HashMap<BlockId, HashMap<IdentifierId, Identifier>>,
        pred_lists: &HashMap<BlockId, Vec<BlockId>>,
        func: &HIRFunction,
        visited: &mut HashSet<BlockId>,
        memo: &mut HashMap<(BlockId, IdentifierId), Option<Identifier>>,
    ) -> Option<Identifier> {
        if !visited.insert(current) {
            return None;
        }
        if let Some(cached) = memo.get(&(current, orig_id)) {
            return cached.clone();
        }

        if let Some(defs) = block_defs.get(&current)
            && let Some(def) = defs.get(&orig_id)
        {
            let result = Some(def.clone());
            memo.insert((current, orig_id), result.clone());
            return result;
        }

        let preds = pred_lists.get(&current).cloned().unwrap_or_else(|| {
            func.body
                .blocks
                .iter()
                .find(|(id, _)| *id == current)
                .map_or_else(Vec::new, |(_, block)| {
                    let mut v: Vec<BlockId> = block.preds.iter().copied().collect();
                    v.sort_by_key(|id| id.0);
                    v
                })
        });
        if preds.is_empty() {
            memo.insert((current, orig_id), None);
            return None;
        }
        if preds.len() == 1 {
            let result = helper(
                preds[0], orig_id, block_defs, pred_lists, func, visited, memo,
            );
            memo.insert((current, orig_id), result.clone());
            return result;
        }

        let mut found: Option<Identifier> = None;
        for pred in preds.iter().rev() {
            let mut branch_visited = visited.clone();
            let Some(def) = helper(
                *pred,
                orig_id,
                block_defs,
                pred_lists,
                func,
                &mut branch_visited,
                memo,
            ) else {
                continue;
            };
            if let Some(existing) = &found {
                if existing.id != def.id {
                    return None;
                }
            } else {
                found = Some(def);
            }
        }
        memo.insert((current, orig_id), found.clone());
        found
    }

    helper(
        start_block,
        orig_id,
        block_defs,
        pred_lists,
        func,
        &mut HashSet::new(),
        &mut memo,
    )
}

struct SSAContext {
    /// Per-block definitions: block_id -> (original_id -> most recent SSA Identifier)
    block_defs: HashMap<BlockId, HashMap<IdentifierId, Identifier>>,
    /// Global definitions: original_id -> most recent SSA Identifier
    global_defs: HashMap<IdentifierId, Identifier>,
    /// Predecessor count per block
    pred_count: HashMap<BlockId, usize>,
    /// Deterministic predecessor lists captured from CFG successor walk.
    pred_lists: HashMap<BlockId, Vec<BlockId>>,
    /// Unsealed predecessor count
    unsealed: HashMap<BlockId, usize>,
    /// Next SSA identifier ID
    next_id: u32,
    /// Currently processing block
    current_block: BlockId,
    /// Visited blocks
    visited: HashSet<BlockId>,
    /// Identifiers that were read before any definition was known.
    unknown_uses: HashMap<IdentifierId, Identifier>,
    /// Deferred error raised when a previously unknown identifier is later defined.
    pending_error: Option<CompilerError>,
    /// Track which (block, orig_id) pairs have instruction-level definitions.
    /// When a phi is placed at a block that already has an instruction-level def
    /// for the same variable, the phi must not overwrite the block's outgoing
    /// definition: the last instruction-level def is what flows OUT of the block
    /// to successor phis.
    instr_defs: HashSet<(BlockId, IdentifierId)>,
}

impl SSAContext {
    fn new(func: &HIRFunction, next_id: u32) -> Self {
        // NOTE: pred_count is NOT computed here because block.preds
        // haven't been populated yet at this point. It will be computed
        // after predecessor edges are built.
        SSAContext {
            block_defs: HashMap::new(),
            global_defs: HashMap::new(),
            pred_count: HashMap::new(),
            pred_lists: HashMap::new(),
            unsealed: HashMap::new(),
            next_id,
            current_block: func.body.entry,
            visited: HashSet::new(),
            unknown_uses: HashMap::new(),
            pending_error: None,
            instr_defs: HashSet::new(),
        }
    }

    fn make_ssa_id(&mut self, old_id: &Identifier) -> Identifier {
        let id = IdentifierId(self.next_id);
        self.next_id += 1;
        Identifier {
            id,
            declaration_id: old_id.declaration_id,
            name: old_id.name.clone(),
            mutable_range: MutableRange::default(),
            scope: None,
            type_: make_type(),
            loc: old_id.loc.clone(),
        }
    }

    fn define(
        &mut self,
        old_id: &Identifier,
        block_id: BlockId,
        allow_undefined_use: bool,
    ) -> Identifier {
        if self.pending_error.is_none()
            && let Some(undefined_ident) = self.unknown_uses.get(&old_id.id).cloned()
        {
            if std::env::var("DEBUG_SSA_HOIST").is_ok() {
                let defined_name = old_id
                    .name
                    .as_ref()
                    .map_or_else(|| "<unknown>".to_string(), |name| name.value().to_string());
                let unknown_name = undefined_ident
                    .name
                    .as_ref()
                    .map_or_else(|| "<unknown>".to_string(), |name| name.value().to_string());
                eprintln!(
                    "[SSA_HOIST] define old={}#{} unknown={}#{} allow_undefined_use={}",
                    defined_name,
                    old_id.id.0,
                    unknown_name,
                    undefined_ident.id.0,
                    allow_undefined_use
                );
            }
            if allow_undefined_use {
                self.unknown_uses.remove(&old_id.id);
                // Preserve identifier identity for hoisted-before-definition uses
                // (notably function declarations) so later def-use passes can
                // still connect the first use to its eventual definition.
                self.set_def(block_id, old_id.id, undefined_ident.clone());
                self.instr_defs.insert((block_id, old_id.id));
                return undefined_ident;
            } else {
                let name = match &undefined_ident.name {
                    Some(IdentifierName::Named(name)) => {
                        format!("{}${}", name, undefined_ident.id.0)
                    }
                    Some(IdentifierName::Promoted(name)) => {
                        format!("{}${}", name, undefined_ident.id.0)
                    }
                    None => format!("<unknown>${}", undefined_ident.id.0),
                };
                self.pending_error = Some(CompilerError::Bail(BailOut {
                    reason:
                        "[hoisting] EnterSSA: Expected identifier to be defined before being used"
                            .to_string(),
                    diagnostics: vec![CompilerDiagnostic {
                        severity: DiagnosticSeverity::Todo,
                        message: format!("Identifier {} is undefined.", name),
                    }],
                }));
            }
        }
        let new_id = self.make_ssa_id(old_id);
        self.set_def(block_id, old_id.id, new_id.clone());
        self.instr_defs.insert((block_id, old_id.id));
        new_id
    }

    fn set_def(&mut self, block_id: BlockId, orig_id: IdentifierId, ssa_id: Identifier) {
        self.block_defs
            .entry(block_id)
            .or_default()
            .insert(orig_id, ssa_id.clone());
        self.global_defs.insert(orig_id, ssa_id);
    }

    /// Set definition from a phi node. Unlike instruction-level definitions,
    /// phi definitions should NOT overwrite the block's outgoing definition
    /// in `block_defs` if an instruction-level definition already exists.
    /// The phi represents the ENTRY value of the variable, while the instruction
    /// definition is the EXIT value that flows to successor blocks.
    fn set_phi_def(&mut self, block_id: BlockId, orig_id: IdentifierId, ssa_id: Identifier) {
        if !self.instr_defs.contains(&(block_id, orig_id)) {
            // No instruction-level def in this block; phi is the outgoing def
            self.block_defs
                .entry(block_id)
                .or_default()
                .insert(orig_id, ssa_id.clone());
        }
        // Always update global_defs so downstream blocks see this phi as a
        // potential reaching def (if they don't have their own definition).
        self.global_defs.insert(orig_id, ssa_id);
    }

    fn lookup(&self, orig_id: IdentifierId, block_id: BlockId) -> Option<Identifier> {
        // Check current block defs first
        if let Some(defs) = self.block_defs.get(&block_id)
            && let Some(def) = defs.get(&orig_id)
        {
            return Some(def.clone());
        }
        // Fall back to global defs
        self.global_defs.get(&orig_id).cloned()
    }

    fn mark_unknown_use(&mut self, identifier: &Identifier) {
        self.unknown_uses
            .entry(identifier.id)
            .or_insert_with(|| identifier.clone());
    }

    fn take_pending_error(&mut self) -> Option<CompilerError> {
        self.pending_error.take()
    }
}
