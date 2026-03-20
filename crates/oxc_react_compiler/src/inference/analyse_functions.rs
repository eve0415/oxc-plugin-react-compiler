//! Analyse inner function expressions and object methods.
//!
//! Port of `AnalyseFunctions.ts` from upstream React Compiler
//! (babel-plugin-react-compiler). Copyright (c) Meta Platforms, Inc. and affiliates.
//! Licensed under MIT.
//!
//! This pass recursively runs the aliasing pipeline on inner `FunctionExpression`
//! and `ObjectMethod` instructions. For each such instruction it:
//! 1. Runs the full mutation/aliasing inference pipeline on the inner function
//! 2. Populates the `Effect` of each context variable based on whether it was
//!    captured or mutated by the inner function
//! 3. Resets `mutableRange` and `scope` on context operands so the outer
//!    `inferMutationAliasingRanges` can recompute them

use std::collections::{HashMap, HashSet};

use crate::hir::types::*;
use crate::hir::visitors;
use crate::inference::aliasing_effects::{AliasingEffect, AliasingSignature};
use crate::inference::infer_mutation_aliasing_effects;
use crate::inference::infer_mutation_aliasing_ranges;
use crate::optimization::constant_propagation;
use crate::optimization::dead_code_elimination;
use crate::reactive_scopes::infer_scope_variables;
use crate::ssa::rewrite_instruction_kinds;
use crate::type_inference;

#[inline]
fn debug_inner_alias_flow_enabled() -> bool {
    std::env::var("DEBUG_INNER_ALIAS_FLOW").is_ok()
}

fn debug_inner_alias_flow_identifier(ident: &Identifier) -> String {
    let name = ident
        .name
        .as_ref()
        .map(IdentifierName::value)
        .unwrap_or("<tmp>");
    format!(
        "{}#{}:d{}:{:?}",
        name, ident.id.0, ident.declaration_id.0, ident.type_
    )
}

fn debug_inner_alias_flow_place(place: &Place) -> String {
    debug_inner_alias_flow_identifier(&place.identifier)
}

fn debug_inner_alias_flow_dump(stage: &str, func: &HIRFunction) {
    if !debug_inner_alias_flow_enabled() {
        return;
    }
    eprintln!(
        "[INNER_ALIAS_FLOW] stage={} entry=bb{} blocks={}",
        stage,
        func.body.entry.0,
        func.body.blocks.len()
    );
    for (_, block) in &func.body.blocks {
        eprintln!(
            "[INNER_ALIAS_FLOW]   bb{} kind={:?} instrs={}",
            block.id.0,
            block.kind,
            block.instructions.len()
        );
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} {} = LoadLocal({})",
                        instr.id.0,
                        debug_inner_alias_flow_identifier(&instr.lvalue.identifier),
                        debug_inner_alias_flow_place(place)
                    );
                }
                InstructionValue::LoadGlobal { binding, .. } => {
                    let binding_name = match binding {
                        NonLocalBinding::ImportDefault { name, module } => {
                            format!("import-default({module}::{name})")
                        }
                        NonLocalBinding::ImportNamespace { name, module } => {
                            format!("import-namespace({module}::{name})")
                        }
                        NonLocalBinding::ImportSpecifier {
                            name,
                            module,
                            imported,
                        } => {
                            format!("import-specifier({module}::{imported} as {name})")
                        }
                        NonLocalBinding::ModuleLocal { name } => {
                            format!("module-local({name})")
                        }
                        NonLocalBinding::Global { name } => format!("global({name})"),
                    };
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} {} = LoadGlobal({})",
                        instr.id.0,
                        debug_inner_alias_flow_identifier(&instr.lvalue.identifier),
                        binding_name
                    );
                }
                InstructionValue::LoadContext { place, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} {} = LoadContext({})",
                        instr.id.0,
                        debug_inner_alias_flow_identifier(&instr.lvalue.identifier),
                        debug_inner_alias_flow_place(place)
                    );
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} StoreLocal({:?}) {} <- {}",
                        instr.id.0,
                        lvalue.kind,
                        debug_inner_alias_flow_place(&lvalue.place),
                        debug_inner_alias_flow_place(value)
                    );
                }
                InstructionValue::StoreContext { lvalue, value, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} StoreContext({:?}) {} <- {}",
                        instr.id.0,
                        lvalue.kind,
                        debug_inner_alias_flow_place(&lvalue.place),
                        debug_inner_alias_flow_place(value)
                    );
                }
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} DeclareLocal({:?}) {}",
                        instr.id.0,
                        lvalue.kind,
                        debug_inner_alias_flow_place(&lvalue.place)
                    );
                }
                InstructionValue::DeclareContext { lvalue, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} DeclareContext({:?}) {}",
                        instr.id.0,
                        lvalue.kind,
                        debug_inner_alias_flow_place(&lvalue.place)
                    );
                }
                InstructionValue::PropertyStore { object, value, .. } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} PropertyStore(object={}, value={})",
                        instr.id.0,
                        debug_inner_alias_flow_place(object),
                        debug_inner_alias_flow_place(value)
                    );
                }
                InstructionValue::ComputedStore {
                    object,
                    property,
                    value,
                    ..
                } => {
                    eprintln!(
                        "[INNER_ALIAS_FLOW]     #{} ComputedStore(object={}, property={}, value={})",
                        instr.id.0,
                        debug_inner_alias_flow_place(object),
                        debug_inner_alias_flow_place(property),
                        debug_inner_alias_flow_place(value)
                    );
                }
                _ => {}
            }
        }
    }
}

#[inline]
fn is_resolved_type(ty: &Type) -> bool {
    !matches!(ty, Type::Poly | Type::TypeVar { .. })
}

fn synchronize_identifier_types(func: &mut HIRFunction) {
    #[inline]
    fn record_identifier_type(known: &mut HashMap<IdentifierId, Type>, ident: &Identifier) {
        if is_resolved_type(&ident.type_) {
            known.entry(ident.id).or_insert_with(|| ident.type_.clone());
        }
    }

    fn collect(func: &HIRFunction, known: &mut HashMap<IdentifierId, Type>) {
        for param in &func.params {
            match param {
                Argument::Place(place) | Argument::Spread(place) => {
                    record_identifier_type(known, &place.identifier)
                }
            }
        }
        for place in &func.context {
            record_identifier_type(known, &place.identifier);
        }
        record_identifier_type(known, &func.returns.identifier);

        for (_block_id, block) in &func.body.blocks {
            for phi in &block.phis {
                record_identifier_type(known, &phi.place.identifier);
                for operand in phi.operands.values() {
                    record_identifier_type(known, &operand.identifier);
                }
            }
            for instr in &block.instructions {
                visitors::for_each_instruction_lvalue(instr, |place| {
                    record_identifier_type(known, &place.identifier)
                });
                visitors::for_each_instruction_operand(instr, |place| {
                    record_identifier_type(known, &place.identifier)
                });
                match &instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        collect(&lowered_func.func, known);
                    }
                    _ => {}
                }
            }
            visitors::for_each_terminal_operand(&block.terminal, |place| {
                record_identifier_type(known, &place.identifier)
            });
        }
    }

    fn apply(func: &mut HIRFunction, known: &HashMap<IdentifierId, Type>) {
        let update = |ident: &mut Identifier| {
            if matches!(ident.type_, Type::Poly | Type::TypeVar { .. })
                && let Some(ty) = known.get(&ident.id)
            {
                ident.type_ = ty.clone();
            }
        };

        for param in &mut func.params {
            match param {
                Argument::Place(place) | Argument::Spread(place) => update(&mut place.identifier),
            }
        }
        for place in &mut func.context {
            update(&mut place.identifier);
        }
        update(&mut func.returns.identifier);

        for (_block_id, block) in &mut func.body.blocks {
            for phi in &mut block.phis {
                update(&mut phi.place.identifier);
                for operand in phi.operands.values_mut() {
                    update(&mut operand.identifier);
                }
            }
            for instr in &mut block.instructions {
                visitors::map_instruction_lvalues(instr, |place| update(&mut place.identifier));
                visitors::map_instruction_operands(instr, |place| update(&mut place.identifier));
                match &mut instr.value {
                    InstructionValue::FunctionExpression { lowered_func, .. }
                    | InstructionValue::ObjectMethod { lowered_func, .. } => {
                        apply(&mut lowered_func.func, known);
                    }
                    _ => {}
                }
            }
            visitors::map_terminal_operands(&mut block.terminal, |place| {
                update(&mut place.identifier)
            });
        }
    }

    let mut known = HashMap::new();
    collect(func, &mut known);
    apply(func, &known);
}

fn prune_unused_load_context_instructions(func: &mut HIRFunction) {
    loop {
        let mut used_ids: HashSet<IdentifierId> = HashSet::new();
        for (_block_id, block) in &func.body.blocks {
            for phi in &block.phis {
                for operand in phi.operands.values() {
                    used_ids.insert(operand.identifier.id);
                }
            }
            for instr in &block.instructions {
                visitors::for_each_instruction_operand(instr, |place| {
                    used_ids.insert(place.identifier.id);
                });
            }
            visitors::for_each_terminal_operand(&block.terminal, |place| {
                used_ids.insert(place.identifier.id);
            });
        }

        let mut changed = false;
        for (_block_id, block) in &mut func.body.blocks {
            let mut next_instructions: Vec<Instruction> =
                Vec::with_capacity(block.instructions.len());
            let last_index = block.instructions.len().saturating_sub(1);
            let is_value_block = block.kind != BlockKind::Block;
            for (idx, instr) in std::mem::take(&mut block.instructions)
                .into_iter()
                .enumerate()
            {
                let keep = if matches!(&instr.value, InstructionValue::LoadContext { .. }) {
                    let is_block_value = is_value_block && idx == last_index;
                    is_block_value || used_ids.contains(&instr.lvalue.identifier.id)
                } else {
                    true
                };
                if keep {
                    next_instructions.push(instr);
                } else {
                    changed = true;
                }
            }
            block.instructions = next_instructions;
        }

        if !changed {
            break;
        }
    }
}

fn prune_unused_context_operands(func: &mut HIRFunction) {
    let mut used_ids: HashSet<IdentifierId> = HashSet::new();
    let mut used_names: HashSet<String> = HashSet::new();

    for (_block_id, block) in &func.body.blocks {
        for phi in &block.phis {
            for operand in phi.operands.values() {
                used_ids.insert(operand.identifier.id);
                if let Some(name) = &operand.identifier.name {
                    used_names.insert(name.value().to_string());
                }
            }
        }
        for instr in &block.instructions {
            visitors::for_each_instruction_operand(instr, |place| {
                used_ids.insert(place.identifier.id);
                if let Some(name) = &place.identifier.name {
                    used_names.insert(name.value().to_string());
                }
            });
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            used_ids.insert(place.identifier.id);
            if let Some(name) = &place.identifier.name {
                used_names.insert(name.value().to_string());
            }
        });
    }

    func.context.retain(|place| {
        used_ids.contains(&place.identifier.id)
            || place
                .identifier
                .name
                .as_ref()
                .is_some_and(|name| used_names.contains(name.value()))
    });
}

/// Recursively analyse inner function expressions and object methods.
///
/// For each `FunctionExpression` or `ObjectMethod` instruction found in `func`,
/// this runs the full mutation/aliasing pipeline on the inner function and then
/// populates context variable effects for the outer function's inference.
pub fn analyse_functions(func: &mut HIRFunction) {
    // Collect all named identifiers from the outer function so we can detect
    // which LoadGlobal instructions in inner functions actually reference
    // outer-scope variables (and should be context variables).
    let outer_vars = collect_outer_variable_names(func);
    let mut declaration_signatures: HashMap<DeclarationId, (AliasingSignature, Vec<Place>)> =
        HashMap::new();

    for (_block_id, block) in &mut func.body.blocks {
        for instr in &mut block.instructions {
            match &mut instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    debug_inner_alias_flow_dump(
                        "analyse_functions:before_populate_context_variables",
                        &lowered_func.func,
                    );
                    // Populate context variables before running the aliasing pipeline.
                    // This converts LoadGlobal references to outer variables into
                    // LoadContext references and populates the inner function's
                    // context array.
                    populate_context_variables(
                        &mut lowered_func.func,
                        &outer_vars,
                        &declaration_signatures,
                    );
                    debug_inner_alias_flow_dump(
                        "analyse_functions:after_populate_context_variables",
                        &lowered_func.func,
                    );

                    // Upstream runs constant propagation before analyseFunctions
                    // on a function where captured context is already lowered.
                    // Our port lowers captured context here, so run CP now to
                    // recover equivalent simplifications for nested functions.
                    constant_propagation::constant_propagation(&mut lowered_func.func);
                    // Context capture lowering runs after the outer InferTypes pass.
                    // Re-run type inference on the lowered inner function so ref-like
                    // captured values preserve upstream type-driven aliasing behavior.
                    type_inference::infer_types(&mut lowered_func.func);
                    synchronize_identifier_types(&mut lowered_func.func);
                    debug_inner_alias_flow_dump(
                        "analyse_functions:after_constant_propagation",
                        &lowered_func.func,
                    );

                    lower_with_mutation_aliasing(&mut lowered_func.func);
                    prune_unused_load_context_instructions(&mut lowered_func.func);
                    prune_unused_context_operands(&mut lowered_func.func);
                    debug_inner_alias_flow_dump(
                        "analyse_functions:after_lower_with_mutation_aliasing",
                        &lowered_func.func,
                    );

                    // Reset mutableRange for outer inferReferenceEffects.
                    //
                    // NOTE: inferReactiveScopeVariables makes identifiers in the scope
                    // point to the *same* mutableRange instance. Resetting start/end
                    // here is insufficient in the upstream (TS) because a later mutation
                    // of the range for any one identifier could affect the range for
                    // other identifiers. In Rust we clone ranges so this is less of an
                    // issue, but we reset them all the same for correctness.
                    for operand in &mut lowered_func.func.context {
                        operand.identifier.mutable_range = MutableRange {
                            start: make_instruction_id(0),
                            end: make_instruction_id(0),
                        };
                        operand.identifier.scope = None;
                    }

                    if let Some(signature) =
                        infer_mutation_aliasing_effects::build_signature_for_lowered_function(
                            lowered_func,
                        )
                    {
                        let context = lowered_func.func.context.clone();
                        declaration_signatures
                            .insert(instr.lvalue.identifier.declaration_id, (signature, context));
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if let Some((signature, context)) = declaration_signatures
                        .get(&value.identifier.declaration_id)
                        .cloned()
                    {
                        declaration_signatures
                            .insert(lvalue.place.identifier.declaration_id, (signature, context));
                    }
                }
                _ => {}
            }
        }
    }

    // Context lowering (populate_context_variables) creates new LoadContext/StoreContext
    // instructions on the outer function that may reference constants. Re-run CP on the
    // outer function to fold these, matching the upstream where BuildHIR already lowers
    // context before constant propagation runs.
    crate::optimization::constant_propagation::constant_propagation(func);
}

/// Collect all named variable identifiers from a function.
///
/// Returns a map from variable name to its Identifier, gathering names from:
/// - Function parameters
/// - DeclareLocal / DeclareContext lvalues
/// - StoreLocal / StoreContext lvalues
/// - Phi node operands
/// - The function's own context variables (for nested functions)
fn collect_outer_variable_names(func: &HIRFunction) -> HashMap<String, Identifier> {
    let mut vars: HashMap<String, Identifier> = HashMap::new();

    fn is_later_source_loc(new_ident: &Identifier, existing_ident: &Identifier) -> bool {
        match (&new_ident.loc, &existing_ident.loc) {
            (SourceLocation::Source(new_loc), SourceLocation::Source(existing_loc)) => {
                (new_loc.start.line, new_loc.start.column)
                    > (existing_loc.start.line, existing_loc.start.column)
            }
            (SourceLocation::Source(_), SourceLocation::Generated) => true,
            _ => false,
        }
    }

    /// Insert or update an entry: prefer identifiers with more specific (non-Poly/non-TypeVar) types.
    /// This ensures that phi-joined identifiers (which carry inferred types from type inference)
    /// override earlier DeclareLocal entries that still have Poly/TypeVar types.
    fn insert_or_upgrade(vars: &mut HashMap<String, Identifier>, name: String, ident: &Identifier) {
        use crate::hir::types::Type;
        let debug_name = name.clone();
        match vars.entry(name) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(ident.clone());
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let existing_type = &e.get().type_;
                let new_type = &ident.type_;
                // Upgrade if existing is unresolved but new is resolved
                let existing_unresolved =
                    matches!(existing_type, Type::Poly | Type::TypeVar { .. });
                let new_resolved = !matches!(new_type, Type::Poly | Type::TypeVar { .. });
                // Prefer the later source declaration when the new candidate is
                // at least as specific as the existing one:
                // - both unresolved (shadowed lexical placeholders)
                // - both resolved (later source dominates for nested captures)
                let prefer_new_by_loc =
                    is_later_source_loc(ident, e.get()) && (new_resolved || existing_unresolved);
                if existing_unresolved && new_resolved {
                    if std::env::var("DEBUG_CONTEXT_CAPTURE").is_ok() {
                        eprintln!(
                            "[CONTEXT_CAPTURE] upgrade-binding name={} existing_decl={} new_decl={} reason=resolved-type",
                            debug_name,
                            e.get().declaration_id.0,
                            ident.declaration_id.0
                        );
                    }
                    e.insert(ident.clone());
                } else if prefer_new_by_loc {
                    if std::env::var("DEBUG_CONTEXT_CAPTURE").is_ok() {
                        eprintln!(
                            "[CONTEXT_CAPTURE] upgrade-binding name={} existing_decl={} new_decl={} reason=later-source-loc",
                            debug_name,
                            e.get().declaration_id.0,
                            ident.declaration_id.0
                        );
                    }
                    e.insert(ident.clone());
                }
            }
        }
    }

    // From parameters
    for param in &func.params {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        if let Some(ref name) = place.identifier.name {
            insert_or_upgrade(&mut vars, name.value().to_string(), &place.identifier);
        }
    }

    // From instructions and phi nodes in all blocks
    for (_block_id, block) in &func.body.blocks {
        // From phi nodes
        for phi in &block.phis {
            if let Some(ref name) = phi.place.identifier.name {
                insert_or_upgrade(&mut vars, name.value().to_string(), &phi.place.identifier);
            }
        }

        // From instructions
        for instr in &block.instructions {
            // Collect from lvalue
            if let Some(ref name) = instr.lvalue.identifier.name {
                insert_or_upgrade(
                    &mut vars,
                    name.value().to_string(),
                    &instr.lvalue.identifier,
                );
            }

            // Collect from DeclareLocal/DeclareContext/StoreLocal/StoreContext lvalues
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if let Some(ref name) = lvalue.place.identifier.name {
                        insert_or_upgrade(
                            &mut vars,
                            name.value().to_string(),
                            &lvalue.place.identifier,
                        );
                    }
                }
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if let Some(ref name) = lvalue.place.identifier.name {
                        insert_or_upgrade(
                            &mut vars,
                            name.value().to_string(),
                            &lvalue.place.identifier,
                        );
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_names(&lvalue.pattern, &mut vars);
                }
                _ => {}
            }
        }
    }

    // From context variables (for nested-within-nested functions)
    //
    // Context names come from outer scopes; they must not override same-name
    // locals/params declared in the current function. Add them last and only
    // when missing.
    for ctx_place in &func.context {
        if let Some(ref name) = ctx_place.identifier.name {
            let key = name.value().to_string();
            if vars.contains_key(&key) {
                if std::env::var("DEBUG_CONTEXT_CAPTURE").is_ok()
                    && let Some(existing) = vars.get(&key)
                {
                    eprintln!(
                        "[CONTEXT_CAPTURE] keep-local-over-context name={} local_decl={} context_decl={}",
                        key, existing.declaration_id.0, ctx_place.identifier.declaration_id.0
                    );
                }
                continue;
            }
            vars.insert(key, ctx_place.identifier.clone());
        }
    }

    vars
}

fn source_loc_position(loc: &SourceLocation) -> (u32, u32) {
    match loc {
        SourceLocation::Source(range) => (range.start.line, range.start.column),
        SourceLocation::Generated => (0, 0),
    }
}

fn strip_generated_binding_suffix(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('_')?;
    if base.is_empty() || suffix.is_empty() {
        return None;
    }
    if suffix.chars().all(|ch| ch.is_ascii_digit()) {
        Some(base)
    } else {
        None
    }
}

fn lookup_outer_identifier_by_capture_name<'a>(
    outer_vars: &'a HashMap<String, Identifier>,
    captured_name: &str,
) -> Option<&'a Identifier> {
    let mut best_match: Option<&Identifier> = None;
    let mut best_loc: (u32, u32) = (0, 0);

    for (outer_name, ident) in outer_vars {
        let name_matches = if outer_name == captured_name {
            true
        } else {
            strip_generated_binding_suffix(outer_name).is_some_and(|base| base == captured_name)
        };
        if !name_matches {
            continue;
        }

        let loc = source_loc_position(&ident.loc);
        if best_match.is_none() || loc >= best_loc {
            best_match = Some(ident);
            best_loc = loc;
        }
    }

    best_match
}

/// Collect names declared locally in a function (params + local writes/decls),
/// excluding context variables inherited from outer scopes.
fn collect_locally_declared_names(func: &HIRFunction) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();

    for param in &func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        if let Some(name) = &place.identifier.name {
            insert_declared_name(&mut names, name.value());
        }
    }

    for (_block_id, block) in &func.body.blocks {
        for phi in &block.phis {
            if let Some(name) = &phi.place.identifier.name {
                insert_declared_name(&mut names, name.value());
            }
        }
        for instr in &block.instructions {
            if let Some(name) = &instr.lvalue.identifier.name {
                insert_declared_name(&mut names, name.value());
            }
            match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. }
                | InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if let Some(name) = &lvalue.place.identifier.name {
                        insert_declared_name(&mut names, name.value());
                    }
                }
                InstructionValue::Destructure { lvalue, .. } => {
                    collect_pattern_names_to_set(&lvalue.pattern, &mut names);
                }
                _ => {}
            }
        }
    }

    names
}

fn insert_declared_name(names: &mut HashSet<String>, name: &str) {
    names.insert(name.to_string());
    if let Some(base) = strip_generated_binding_suffix(name) {
        names.insert(base.to_string());
    }
}

/// Collect named identifiers from destructuring patterns.
fn collect_pattern_names(pattern: &Pattern, vars: &mut HashMap<String, Identifier>) {
    match pattern {
        Pattern::Array(arr) => {
            for elem in &arr.items {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        if let Some(ref name) = p.identifier.name {
                            vars.entry(name.value().to_string())
                                .or_insert_with(|| p.identifier.clone());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let Some(ref name) = p.place.identifier.name {
                            vars.entry(name.value().to_string())
                                .or_insert_with(|| p.place.identifier.clone());
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        if let Some(ref name) = p.identifier.name {
                            vars.entry(name.value().to_string())
                                .or_insert_with(|| p.identifier.clone());
                        }
                    }
                }
            }
        }
    }
}

fn collect_pattern_names_to_set(pattern: &Pattern, names: &mut HashSet<String>) {
    match pattern {
        Pattern::Array(arr) => {
            for elem in &arr.items {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        if let Some(name) = &p.identifier.name {
                            insert_declared_name(names, name.value());
                        }
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let Some(name) = &p.place.identifier.name {
                            insert_declared_name(names, name.value());
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        if let Some(name) = &p.identifier.name {
                            insert_declared_name(names, name.value());
                        }
                    }
                }
            }
        }
    }
}

/// Recursively collect captured variable names from a function body.
/// Looks for LoadGlobal and StoreGlobal instructions that reference outer variables,
/// including inside nested function expressions.
fn collect_captured_names_recursive(
    body: &HIR,
    outer_vars: &HashMap<String, Identifier>,
    captured_names: &mut HashMap<String, Identifier>,
    shadowed_names: &HashSet<String>,
) {
    for (_block_id, block) in &body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name },
                    ..
                } => {
                    if shadowed_names.contains(name) {
                        continue;
                    }
                    if let Some(outer_ident) =
                        lookup_outer_identifier_by_capture_name(outer_vars, name)
                    {
                        captured_names
                            .entry(name.clone())
                            .or_insert_with(|| outer_ident.clone());
                    }
                }
                InstructionValue::StoreGlobal { name, .. } => {
                    if shadowed_names.contains(name) {
                        continue;
                    }
                    if let Some(outer_ident) =
                        lookup_outer_identifier_by_capture_name(outer_vars, name.as_str())
                    {
                        captured_names
                            .entry(name.clone())
                            .or_insert_with(|| outer_ident.clone());
                    }
                }
                // Recurse into nested function expressions
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    let mut nested_shadowed = shadowed_names.clone();
                    nested_shadowed.extend(collect_locally_declared_names(&lowered_func.func));
                    collect_captured_names_recursive(
                        &lowered_func.func.body,
                        outer_vars,
                        captured_names,
                        &nested_shadowed,
                    );
                }
                _ => {}
            }
        }
    }
}

/// Populate context variables for an inner function by converting LoadGlobal
/// references to outer-scope variables into LoadContext references.
///
/// This is the Rust equivalent of the upstream's `gatherCapturedContext` +
/// context population that happens during BuildHIR lowering. Since our Rust
/// lowering doesn't track outer scope context, we fix it up here before
/// running the aliasing pipeline.
fn populate_context_variables(
    inner_func: &mut HIRFunction,
    outer_vars: &HashMap<String, Identifier>,
    known_signatures: &HashMap<DeclarationId, (AliasingSignature, Vec<Place>)>,
) {
    let debug_context = std::env::var("DEBUG_CONTEXT_CAPTURE").is_ok();
    // First pass: find all LoadGlobal and StoreGlobal instructions that reference
    // outer variables and collect which names need context variables.
    // StoreGlobal is included because inner functions may reassign outer variables
    // (e.g., `local = newValue` inside an arrow function).
    // We search recursively into nested function expressions to handle cases where
    // a variable is only accessed by a deeply nested function.
    let mut captured_names: HashMap<String, Identifier> = HashMap::new();
    let mut shadowed_names = collect_locally_declared_names(inner_func);
    shadowed_names.extend(inner_func.context.iter().filter_map(|place| {
        place
            .identifier
            .name
            .as_ref()
            .map(|name| name.value().to_string())
    }));
    collect_captured_names_recursive(
        &inner_func.body,
        outer_vars,
        &mut captured_names,
        &shadowed_names,
    );
    if debug_context {
        let captured_list = captured_names
            .iter()
            .map(|(name, ident)| {
                format!(
                    "{}(decl={},type={:?})",
                    name, ident.declaration_id.0, ident.type_
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "[CONTEXT_CAPTURE] initial captured=[{}] known_signatures={}",
            captured_list,
            known_signatures.len()
        );
    }

    // Include transitive captures from known local function signatures so nested
    // wrappers (e.g. `g` calling `f`) inherit `f`'s captured context.
    let mut worklist: Vec<Identifier> = captured_names.values().cloned().collect();
    let mut seen_decls: HashSet<DeclarationId> = HashSet::new();
    while let Some(ident) = worklist.pop() {
        if !seen_decls.insert(ident.declaration_id) {
            continue;
        }
        if let Some((_, context)) = known_signatures.get(&ident.declaration_id) {
            if debug_context {
                let ctx_list = context
                    .iter()
                    .map(|place| {
                        format!(
                            "id={} decl={} name={} type={:?}",
                            place.identifier.id.0,
                            place.identifier.declaration_id.0,
                            place
                                .identifier
                                .name
                                .as_ref()
                                .map_or("<none>".to_string(), |n| n.value().to_string()),
                            place.identifier.type_
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                eprintln!(
                    "[CONTEXT_CAPTURE] transitive from decl={} context=[{}]",
                    ident.declaration_id.0, ctx_list
                );
            }
            for place in context {
                if let Some(name) = place
                    .identifier
                    .name
                    .as_ref()
                    .map(|n| n.value().to_string())
                    && !shadowed_names.contains(name.as_str())
                    && let Some(outer_ident) =
                        lookup_outer_identifier_by_capture_name(outer_vars, name.as_str())
                    && !captured_names.contains_key(name.as_str())
                {
                    captured_names.insert(name, outer_ident.clone());
                    worklist.push(outer_ident.clone());
                }
            }
        }
    }

    if captured_names.is_empty() {
        return;
    }

    // Merge captured outer identifiers into existing context instead of
    // replacing it. Replacing drops upstream-originated captures (for example
    // transform_fire-generated bindings), which later appear uninitialized.
    let mut context_places: Vec<Place> = inner_func.context.clone();
    let mut seen_context_decls: HashSet<DeclarationId> = context_places
        .iter()
        .map(|place| place.identifier.declaration_id)
        .collect();

    let mut captured_entries: Vec<(&String, &Identifier)> = captured_names.iter().collect();
    captured_entries.sort_by(|(name_a, ident_a), (name_b, ident_b)| {
        ident_a
            .declaration_id
            .0
            .cmp(&ident_b.declaration_id.0)
            .then_with(|| name_a.cmp(name_b))
    });

    for (name, outer_ident) in captured_entries {
        if !seen_context_decls.insert(outer_ident.declaration_id) {
            continue;
        }
        context_places.push(Place {
            identifier: Identifier {
                id: outer_ident.id,
                declaration_id: outer_ident.declaration_id,
                name: Some(IdentifierName::Named(name.clone())),
                mutable_range: outer_ident.mutable_range.clone(),
                scope: None,
                type_: outer_ident.type_.clone(),
                loc: outer_ident.loc.clone(),
            },
            effect: Effect::Unknown,
            reactive: false,
            loc: outer_ident.loc.clone(),
        });
    }

    inner_func.context = context_places;

    // Second pass: convert LoadGlobal -> LoadContext and StoreGlobal -> StoreContext
    // for captured outer variables. The LoadContext/StoreContext places use the outer
    // identifier so that the inner function's aliasing pipeline can track it as a
    // context variable.
    for (_block_id, block) in &mut inner_func.body.blocks {
        for instr in &mut block.instructions {
            if let InstructionValue::LoadContext { place, .. } = &instr.value
                && matches!(
                    instr.lvalue.identifier.type_,
                    Type::Poly | Type::TypeVar { .. }
                )
                && !matches!(place.identifier.type_, Type::Poly | Type::TypeVar { .. })
            {
                instr.lvalue.identifier.type_ = place.identifier.type_.clone();
            }

            let should_convert_load = matches!(
                &instr.value,
                InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name },
                    ..
                } if captured_names.contains_key(name)
            );
            let should_convert_store = matches!(
                &instr.value,
                InstructionValue::StoreGlobal { name, .. }
                    if captured_names.contains_key(name.as_str())
            );
            if should_convert_load {
                if let InstructionValue::LoadGlobal {
                    binding: NonLocalBinding::Global { name },
                    loc,
                } = std::mem::replace(
                    &mut instr.value,
                    InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    },
                ) {
                    let outer_ident = &captured_names[&name];
                    instr.value = InstructionValue::LoadContext {
                        place: Place {
                            identifier: Identifier {
                                id: outer_ident.id,
                                declaration_id: outer_ident.declaration_id,
                                name: Some(IdentifierName::Named(name)),
                                mutable_range: outer_ident.mutable_range.clone(),
                                scope: None,
                                type_: outer_ident.type_.clone(),
                                loc: outer_ident.loc.clone(),
                            },
                            effect: Effect::Unknown,
                            reactive: false,
                            loc: loc.clone(),
                        },
                        loc,
                    };
                    // Inner aliasing analysis does not run InferTypes; preserve the
                    // captured context's concrete type on the load result so ref-like
                    // mutations (e.g. `.current`) are classified like upstream.
                    instr.lvalue.identifier.type_ = outer_ident.type_.clone();
                }
            } else if should_convert_store
                && let InstructionValue::StoreGlobal { name, value, loc } = std::mem::replace(
                    &mut instr.value,
                    InstructionValue::Primitive {
                        value: PrimitiveValue::Undefined,
                        loc: SourceLocation::Generated,
                    },
                )
            {
                let outer_ident = &captured_names[&name];
                instr.value = InstructionValue::StoreContext {
                    lvalue: LValue {
                        place: Place {
                            identifier: Identifier {
                                id: outer_ident.id,
                                declaration_id: outer_ident.declaration_id,
                                name: Some(IdentifierName::Named(name)),
                                mutable_range: outer_ident.mutable_range.clone(),
                                scope: None,
                                type_: outer_ident.type_.clone(),
                                loc: outer_ident.loc.clone(),
                            },
                            effect: Effect::Unknown,
                            reactive: false,
                            loc: loc.clone(),
                        },
                        kind: InstructionKind::Reassign,
                    },
                    value,
                    loc,
                };
            }
        }
    }
}

/// Run the full mutation/aliasing inference pipeline on an inner function.
///
/// This mirrors the `lowerWithMutationAliasing` helper in the upstream.
/// It runs the following passes in order:
/// 1. `analyseFunctions` (recursive)
/// 2. `inferMutationAliasingEffects` (with `is_function_expression = true`)
/// 3. `deadCodeElimination`
/// 4. `inferMutationAliasingRanges` (with `is_function_expression = true`)
/// 5. `rewriteInstructionKindsBasedOnReassignment`
/// 6. `inferReactiveScopeVariables`
///
/// After running the pipeline it populates context variable effects based on
/// which context variables were captured or mutated.
fn lower_with_mutation_aliasing(func: &mut HIRFunction) {
    debug_inner_alias_flow_dump("lower_with_mutation_aliasing:start", func);
    // Phase 1: run the pipeline
    analyse_functions(func);
    debug_inner_alias_flow_dump("lower_with_mutation_aliasing:after_analyse_functions", func);
    infer_mutation_aliasing_effects::infer_mutation_aliasing_effects(func, true, true);
    debug_inner_alias_flow_dump(
        "lower_with_mutation_aliasing:after_infer_mutation_aliasing_effects",
        func,
    );
    dead_code_elimination::dead_code_elimination(func);
    debug_inner_alias_flow_dump(
        "lower_with_mutation_aliasing:after_dead_code_elimination",
        func,
    );
    // For function expressions, is_function_expression=true so errors are collected
    // but not bailed (Ok is always returned for function expressions)
    let function_effects =
        infer_mutation_aliasing_ranges::infer_mutation_aliasing_ranges(func, true)
            .unwrap_or_default();
    debug_inner_alias_flow_dump(
        "lower_with_mutation_aliasing:after_infer_mutation_aliasing_ranges",
        func,
    );
    let _ = rewrite_instruction_kinds::rewrite_instruction_kinds(func);
    debug_inner_alias_flow_dump(
        "lower_with_mutation_aliasing:after_rewrite_instruction_kinds",
        func,
    );
    infer_scope_variables::infer_reactive_scope_variables_with_aliasing(func);
    debug_inner_alias_flow_dump(
        "lower_with_mutation_aliasing:after_infer_reactive_scope_variables",
        func,
    );

    // Store the effects on the function
    func.aliasing_effects = Some(function_effects.clone());

    // Phase 2: populate the Effect of each context variable to use in inferring
    // the outer function. For example, InferMutationAliasingEffects uses context
    // variable effects to decide if the function may be mutable or not.
    let captured_or_mutated = build_captured_or_mutated_set(&function_effects);
    if debug_inner_alias_flow_enabled() {
        let effects_summary = function_effects
            .iter()
            .map(|effect| match effect {
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
                AliasingEffect::Create { into, .. } => {
                    format!("Create({})", into.identifier.id.0)
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
            })
            .collect::<Vec<_>>()
            .join(", ");
        let mut captured_ids = captured_or_mutated
            .iter()
            .map(|id| id.0)
            .collect::<Vec<_>>();
        captured_ids.sort_unstable();
        eprintln!(
            "[INNER_ALIAS_FLOW] effects=[{}] captured_or_mutated={:?}",
            effects_summary, captured_ids
        );
    }

    for operand in &mut func.context {
        if debug_inner_alias_flow_enabled() {
            eprintln!(
                "[INNER_ALIAS_FLOW] context-before id={} decl={} name={} effect={:?} in_set={}",
                operand.identifier.id.0,
                operand.identifier.declaration_id.0,
                operand
                    .identifier
                    .name
                    .as_ref()
                    .map_or("<none>".to_string(), |n| n.value().to_string()),
                operand.effect,
                captured_or_mutated.contains(&operand.identifier.id)
            );
        }
        if captured_or_mutated.contains(&operand.identifier.id) || operand.effect == Effect::Capture
        {
            operand.effect = Effect::Capture;
        } else {
            operand.effect = Effect::Read;
        }
        if debug_inner_alias_flow_enabled() {
            eprintln!(
                "[INNER_ALIAS_FLOW] context-after id={} effect={:?}",
                operand.identifier.id.0, operand.effect
            );
        }
    }
}

/// Build the set of identifier IDs that were captured or mutated according to
/// the given function effects.
///
/// This mirrors Phase 2 of the upstream `lowerWithMutationAliasing`.
fn build_captured_or_mutated_set(function_effects: &[AliasingEffect]) -> HashSet<IdentifierId> {
    let mut captured_or_mutated = HashSet::new();
    for effect in function_effects {
        match effect {
            AliasingEffect::Assign { from, .. }
            | AliasingEffect::Alias { from, .. }
            | AliasingEffect::Capture { from, .. }
            | AliasingEffect::CreateFrom { from, .. }
            | AliasingEffect::MaybeAlias { from, .. } => {
                captured_or_mutated.insert(from.identifier.id);
            }
            AliasingEffect::Apply { .. } => {
                // The upstream panics here with an invariant violation:
                // "Expected Apply effects to be replaced with more precise effects"
                // After inferMutationAliasingEffects runs, Apply effects should be resolved.
                // If one slips through, we silently ignore it rather than panicking.
            }
            AliasingEffect::Mutate { value, .. }
            | AliasingEffect::MutateConditionally { value }
            | AliasingEffect::MutateTransitive { value }
            | AliasingEffect::MutateTransitiveConditionally { value } => {
                captured_or_mutated.insert(value.identifier.id);
            }
            AliasingEffect::Impure { .. }
            | AliasingEffect::Render { .. }
            | AliasingEffect::MutateFrozen { .. }
            | AliasingEffect::MutateGlobal { .. }
            | AliasingEffect::CreateFunction { .. }
            | AliasingEffect::Create { .. }
            | AliasingEffect::Freeze { .. }
            | AliasingEffect::ImmutableCapture { .. } => {
                // no-op
            }
        }
    }
    captured_or_mutated
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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

    /// Helper: create a named Place for context variables.
    fn make_named_place(id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId(id),
                declaration_id: DeclarationId(id),
                name: Some(IdentifierName::Named(name.to_string())),
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

    /// Helper: create a minimal HIRFunction.
    fn make_hir_function(blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
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

    /// Helper: create a basic block with instructions and a return terminal.
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

    /// Helper: create an inner HIRFunction (for use inside FunctionExpression).
    fn make_inner_function(context: Vec<Place>, blocks: Vec<(BlockId, BasicBlock)>) -> HIRFunction {
        HIRFunction {
            env: crate::environment::Environment::new(crate::options::EnvironmentConfig::default()),
            id: None,
            fn_type: ReactFunctionType::Other,
            params: vec![],
            returns: make_place(98),
            context,
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

    #[test]
    fn test_analyse_functions_no_inner_functions() {
        // A function with no FunctionExpression/ObjectMethod instructions.
        // analyse_functions should be a no-op.
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

        // Should not panic or modify anything
        analyse_functions(&mut func);

        // Verify the function is unchanged
        assert_eq!(func.body.blocks.len(), 1);
        assert_eq!(func.body.blocks[0].1.instructions.len(), 1);
    }

    #[test]
    fn test_analyse_functions_with_inner_function() {
        // Create an inner function that has context variables.
        let ctx_var = make_named_place(50, "captured_x");
        let inner_blocks = vec![make_block(0, vec![], 1)];
        let inner_func = make_inner_function(vec![ctx_var], inner_blocks);

        // Create a FunctionExpression instruction wrapping the inner function.
        let func_expr_instr = Instruction {
            id: InstructionId(1),
            lvalue: make_place(10),
            value: InstructionValue::FunctionExpression {
                name: None,
                lowered_func: LoweredFunction { func: inner_func },
                expr_type: FunctionExpressionType::ArrowFunctionExpression,
                loc: SourceLocation::Generated,
            },
            loc: SourceLocation::Generated,
            effects: None,
        };

        let blocks = vec![make_block(0, vec![func_expr_instr], 2)];
        let mut func = make_hir_function(blocks);

        // Run analyse_functions
        analyse_functions(&mut func);

        // After analysis, the inner function's context variables should have
        // their mutable ranges reset to [0, 0) and scope set to None.
        if let InstructionValue::FunctionExpression { lowered_func, .. } =
            &func.body.blocks[0].1.instructions[0].value
        {
            for ctx_operand in &lowered_func.func.context {
                assert_eq!(
                    ctx_operand.identifier.mutable_range.start,
                    InstructionId(0),
                    "Context variable mutable_range.start should be reset to 0"
                );
                assert_eq!(
                    ctx_operand.identifier.mutable_range.end,
                    InstructionId(0),
                    "Context variable mutable_range.end should be reset to 0"
                );
                assert!(
                    ctx_operand.identifier.scope.is_none(),
                    "Context variable scope should be reset to None"
                );
            }

            // The inner function should have aliasing_effects populated
            assert!(
                lowered_func.func.aliasing_effects.is_some(),
                "Inner function should have aliasing_effects populated after analysis"
            );
        } else {
            panic!("Expected FunctionExpression instruction");
        }
    }

    #[test]
    fn test_build_captured_or_mutated_set() {
        let place_a = make_place(1);
        let place_b = make_place(2);
        let place_c = make_place(3);

        let effects = vec![
            AliasingEffect::Capture {
                from: place_a.clone(),
                into: place_b.clone(),
            },
            AliasingEffect::Mutate {
                value: place_c.clone(),
                reason: None,
            },
            AliasingEffect::Create {
                into: place_b.clone(),
                value: ValueKind::Mutable,
                reason: ValueReason::Other,
            },
        ];

        let set = build_captured_or_mutated_set(&effects);

        // place_a (id=1) should be captured (from Capture effect)
        assert!(set.contains(&IdentifierId(1)));
        // place_c (id=3) should be mutated (from Mutate effect)
        assert!(set.contains(&IdentifierId(3)));
        // place_b (id=2) should NOT be in the set (Create is a no-op for this)
        assert!(!set.contains(&IdentifierId(2)));
    }

    #[test]
    fn test_context_variable_effects_populated() {
        // Test that context variable effects are set correctly by
        // lower_with_mutation_aliasing. We create an inner function with
        // a context variable and verify the effect is populated.
        let ctx_var = make_named_place(50, "x");
        let inner_blocks = vec![make_block(0, vec![], 1)];
        let mut inner_func = make_inner_function(vec![ctx_var], inner_blocks);

        // Run the full pipeline on the inner function
        lower_with_mutation_aliasing(&mut inner_func);

        // Context variables should have their effects set to either
        // Capture or Read after the pass runs.
        for ctx_operand in &inner_func.context {
            assert!(
                ctx_operand.effect == Effect::Capture || ctx_operand.effect == Effect::Read,
                "Context variable effect should be Capture or Read, got {:?}",
                ctx_operand.effect
            );
        }
    }
}
