//! Codegen — generate JavaScript from compiled HIR.
//!
//! Simplified port of `CodegenReactiveFunction.ts` from upstream.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! For Milestone 1, this implements a simplified codegen that generates the
//! memoized function pattern for simple components.

use std::collections::{HashMap, HashSet};

use crate::hir::builder::terminal_successors;
use crate::hir::types::*;

/// Extract all input Place identifier IDs from an instruction value.
/// Used for taint propagation in escape analysis.
fn instruction_input_place_ids(value: &InstructionValue) -> Vec<IdentifierId> {
    let mut ids = Vec::new();
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            ids.push(place.identifier.id);
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::Destructure { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::BinaryExpression { .. } => {
            // Binary expressions always produce primitives — no taint propagation
        }
        InstructionValue::UnaryExpression { .. } => {
            // Unary expressions always produce primitives — no taint propagation
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            ids.push(callee.identifier.id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => ids.push(p.identifier.id),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            ids.push(receiver.identifier.id);
            ids.push(property.identifier.id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => ids.push(p.identifier.id),
                }
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            ids.push(callee.identifier.id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => ids.push(p.identifier.id),
                }
            }
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        ids.push(p.place.identifier.id);
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            ids.push(place.identifier.id);
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        ids.push(p.identifier.id);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                        ids.push(p.identifier.id);
                    }
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
                ids.push(p.identifier.id);
            }
            for attr in props {
                match attr {
                    JsxAttribute::Attribute { place, .. } => ids.push(place.identifier.id),
                    JsxAttribute::SpreadAttribute { argument } => ids.push(argument.identifier.id),
                }
            }
            if let Some(children) = children {
                for child in children {
                    ids.push(child.identifier.id);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                ids.push(child.identifier.id);
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            ids.push(object.identifier.id);
        }
        InstructionValue::PropertyStore { object, value, .. } => {
            ids.push(object.identifier.id);
            ids.push(value.identifier.id);
        }
        InstructionValue::PropertyDelete { .. } => {
            // Delete always returns boolean — no taint propagation
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            ids.push(object.identifier.id);
            ids.push(property.identifier.id);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            ids.push(object.identifier.id);
            ids.push(property.identifier.id);
            ids.push(value.identifier.id);
        }
        InstructionValue::ComputedDelete { .. } => {
            // Delete always returns boolean — no taint propagation
        }
        InstructionValue::StoreGlobal { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            ids.push(tag.identifier.id);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                ids.push(expr.identifier.id);
            }
        }
        InstructionValue::TypeCastExpression { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::Await { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::GetIterator { collection, .. } => {
            ids.push(collection.identifier.id);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            ids.push(iterator.identifier.id);
            ids.push(collection.identifier.id);
        }
        InstructionValue::NextPropertyOf { value, .. } => {
            ids.push(value.identifier.id);
        }
        InstructionValue::PrefixUpdate { lvalue, value, .. }
        | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
            ids.push(lvalue.identifier.id);
            ids.push(value.identifier.id);
        }
        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            // Don't propagate from test — truthiness check doesn't taint result.
            // Only consequent/alternate values flow to the result.
            ids.push(consequent.identifier.id);
            ids.push(alternate.identifier.id);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            // Both sides of logical expressions can be the result value
            // (e.g., `a || b` returns a or b, `a && b` returns a or b)
            ids.push(left.identifier.id);
            ids.push(right.identifier.id);
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            ids.push(decl.identifier.id);
        }
        // No input places
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::RegExpLiteral { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::ObjectMethod { .. }
        | InstructionValue::FunctionExpression { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. }
        | InstructionValue::Debugger { .. } => {}
    }
    ids
}

/// Result of code generation for a function.
#[derive(Clone)]
pub struct CodegenResult {
    /// The generated function body code.
    pub body: String,
    /// Number of cache slots needed.
    pub cache_size: u32,
    /// Whether the function needs the cache import.
    pub needs_cache_import: bool,
    /// The next available temp ID (for parameter destructuring in pipeline).
    pub next_temp_id: usize,
    /// Outlined function declarations (name, params_str, body_str).
    /// These should be emitted as `function _name(params) { body }` after the main function.
    pub outlined_functions: Vec<(String, String, String)>,
}

/// Generate code for a compiled function.
pub fn codegen_function(func: &HIRFunction) -> CodegenResult {
    codegen_function_with_temp_start(func, 0)
}

/// Generate code for a compiled function, starting temp naming from `temp_start`.
pub fn codegen_function_with_temp_start(func: &HIRFunction, temp_start: usize) -> CodegenResult {
    codegen_function_with_options(func, temp_start, false)
}

/// Generate code with option to skip memoization.
pub fn codegen_function_with_options(
    func: &HIRFunction,
    temp_start: usize,
    skip_memo: bool,
) -> CodegenResult {
    codegen_function_full(func, temp_start, skip_memo, &HashSet::new())
}

/// Generate code with full options including source variable names for conflict detection.
pub fn codegen_function_full(
    func: &HIRFunction,
    temp_start: usize,
    skip_memo: bool,
    source_names: &HashSet<String>,
) -> CodegenResult {
    let mut cg = CodeGenerator::new();
    cg.temp_start = temp_start;
    cg.skip_memo = skip_memo;
    // Resolve naming conflicts: if source uses "$" as a variable, use "$0" for cache
    if source_names.contains("$") {
        cg.cache_var = "$0".to_string();
    }
    cg.analyze(func);

    cg.emit_function(func);

    let next_temp = cg.temp_start + if cg.promoted_var.is_some() { 0 } else { 1 };
    CodegenResult {
        body: cg.output,
        cache_size: cg.cache_slots,
        needs_cache_import: cg.cache_slots > 0,
        next_temp_id: next_temp,
        outlined_functions: cg.outlined_functions,
    }
}

/// Generate code for an outlined function, returning (params_str, body_str).
///
/// Unlike normal codegen, this does NOT skip param destructuring — the destructuring
/// statements are emitted as part of the function body. Parameters are named using
/// the same sequential temp naming (t0, t1, ...) that the body codegen uses.
pub fn codegen_outlined_fn(func: &HIRFunction) -> (String, String) {
    let mut cg = CodeGenerator::new();
    cg.is_outlined = true;
    cg.skip_memo = true;

    // Pre-assign sequential temp names (t0, t1, ...) to unnamed params BEFORE
    // analyze(), so they get the lowest temp indices. This mirrors how
    // generate_inner_function_body handles params for nested functions.
    for param in &func.params {
        let (ident, has_name) = match param {
            Argument::Place(p) => (&p.identifier, p.identifier.name.is_some()),
            Argument::Spread(p) => (&p.identifier, p.identifier.name.is_some()),
        };
        if !has_name {
            cg.assign_temp_name(ident.id);
        }
    }

    cg.analyze(func);

    // Build param names: for each param, use the name if it has one,
    // otherwise look up the temp name assigned above.
    let params: Vec<String> = func
        .params
        .iter()
        .map(|p| {
            let (ident, is_spread) = match p {
                Argument::Place(place) => (&place.identifier, false),
                Argument::Spread(place) => (&place.identifier, true),
            };
            let name = match &ident.name {
                Some(IdentifierName::Named(name)) => name.clone(),
                Some(IdentifierName::Promoted(name)) => name.clone(),
                None => cg
                    .temp_name_map
                    .get(&ident.id)
                    .cloned()
                    .unwrap_or_else(|| format!("_t{}", ident.id.0)),
            };
            if is_spread {
                format!("...{}", name)
            } else {
                name
            }
        })
        .collect();
    let params_str = params.join(", ");

    cg.emit_function(func);
    let fallback_return = infer_outlined_return_expr(func, &cg);
    let mut body = cg.output;

    // Port recovery: the legacy emitter can drop a terminal return for some
    // outlined CFG shapes (e.g. loop + fallthrough return block). If no return
    // was emitted but HIR has an explicit return, append it from HIR.
    if !body.contains("return ")
        && let Some(ret) = fallback_return
        && ret != "undefined"
    {
        if !body.ends_with('\n') && !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("return {};", ret));
    }

    // Prepend directives (e.g., "worklet") to the outlined function body
    if !func.directives.is_empty() {
        let mut with_directives = String::new();
        for directive in &func.directives {
            with_directives.push_str(&format!("  \"{}\";\n", directive));
        }
        with_directives.push_str(&body);
        return (params_str, with_directives);
    }

    (params_str, body)
}

fn infer_outlined_return_expr(func: &HIRFunction, cg: &CodeGenerator) -> Option<String> {
    for (_, block) in &func.body.blocks {
        if let Terminal::Return {
            value,
            return_variant,
            ..
        } = &block.terminal
            && matches!(
                return_variant,
                ReturnVariant::Explicit | ReturnVariant::Implicit
            )
        {
            return Some(cg.resolve_place(value));
        }
    }
    None
}

/// Collect scope info: for each surviving scope, its ID, range, deps, and declarations.
#[derive(Debug, Clone)]
struct ScopeCodegenInfo {
    id: ScopeId,
    range: MutableRange,
    deps: Vec<String>,
    decl_names: Vec<String>,
}

fn collect_scope_codegen_info(func: &HIRFunction, cg: &CodeGenerator) -> Vec<ScopeCodegenInfo> {
    let mut scope_map: HashMap<ScopeId, ScopeCodegenInfo> = HashMap::new();

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                if scope_map.contains_key(&scope.id) {
                    continue;
                }
                // Render deps as strings
                let deps: Vec<String> = scope
                    .dependencies
                    .iter()
                    .map(|dep| {
                        let base = if let Some(IdentifierName::Named(n)) = &dep.identifier.name {
                            n.clone()
                        } else {
                            cg.resolve_identifier_name(&dep.identifier)
                        };
                        if dep.path.is_empty() {
                            base
                        } else {
                            let path_str: String = dep
                                .path
                                .iter()
                                .map(|p| {
                                    if p.optional {
                                        format!("?.{}", p.property)
                                    } else {
                                        format!(".{}", p.property)
                                    }
                                })
                                .collect();
                            format!("{}{}", base, path_str)
                        }
                    })
                    .collect();

                // Collect declarations (scope outputs used outside)
                let decl_names: Vec<String> = scope
                    .declarations
                    .values()
                    .map(|decl| {
                        if let Some(IdentifierName::Named(n)) = &decl.identifier.name {
                            n.clone()
                        } else {
                            cg.resolve_identifier_name(&decl.identifier)
                        }
                    })
                    .collect();

                scope_map.insert(
                    scope.id,
                    ScopeCodegenInfo {
                        id: scope.id,
                        range: scope.range.clone(),
                        deps,
                        decl_names,
                    },
                );
            }
        }
    }

    // Sort by range start for emission order
    let mut scopes: Vec<ScopeCodegenInfo> = scope_map.into_values().collect();
    scopes.sort_by_key(|s| s.range.start);
    scopes
}

/// Flood-fill from a loop body block to find all blocks reachable within the loop.
/// Build the set of IdentifierIds that are READ by any instruction or terminal.
/// An id in this set means *some instruction consumes the value produced by it*.
/// IDs NOT in this set correspond to instructions whose results are unused.
fn build_consumed_ids(func: &HIRFunction) -> HashSet<IdentifierId> {
    let mut ids = HashSet::new();

    fn add_place(ids: &mut HashSet<IdentifierId>, p: &Place) {
        ids.insert(p.identifier.id);
    }
    fn add_arg(ids: &mut HashSet<IdentifierId>, a: &Argument) {
        match a {
            Argument::Place(p) | Argument::Spread(p) => add_place(ids, p),
        }
    }

    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => add_place(&mut ids, place),

                InstructionValue::StoreLocal { value, .. }
                | InstructionValue::StoreContext { value, .. } => add_place(&mut ids, value),

                InstructionValue::Destructure { value, .. } => add_place(&mut ids, value),

                InstructionValue::BinaryExpression { left, right, .. } => {
                    add_place(&mut ids, left);
                    add_place(&mut ids, right);
                }
                InstructionValue::UnaryExpression { value, .. } => add_place(&mut ids, value),

                InstructionValue::CallExpression { callee, args, .. } => {
                    add_place(&mut ids, callee);
                    for a in args {
                        add_arg(&mut ids, a);
                    }
                }
                InstructionValue::MethodCall {
                    receiver,
                    property,
                    args,
                    ..
                } => {
                    add_place(&mut ids, receiver);
                    add_place(&mut ids, property);
                    for a in args {
                        add_arg(&mut ids, a);
                    }
                }
                InstructionValue::NewExpression { callee, args, .. } => {
                    add_place(&mut ids, callee);
                    for a in args {
                        add_arg(&mut ids, a);
                    }
                }
                InstructionValue::TypeCastExpression { value, .. } => add_place(&mut ids, value),

                InstructionValue::ObjectExpression { properties, .. } => {
                    for prop in properties {
                        match prop {
                            ObjectPropertyOrSpread::Property(p) => add_place(&mut ids, &p.place),
                            ObjectPropertyOrSpread::Spread(p) => add_place(&mut ids, p),
                        }
                    }
                }
                InstructionValue::ArrayExpression { elements, .. } => {
                    for el in elements {
                        match el {
                            ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                                add_place(&mut ids, p)
                            }
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
                        add_place(&mut ids, p);
                    }
                    for attr in props {
                        match attr {
                            JsxAttribute::Attribute { place, .. } => add_place(&mut ids, place),
                            JsxAttribute::SpreadAttribute { argument } => {
                                add_place(&mut ids, argument)
                            }
                        }
                    }
                    if let Some(ch) = children {
                        for p in ch {
                            add_place(&mut ids, p);
                        }
                    }
                }
                InstructionValue::JsxFragment { children, .. } => {
                    for p in children {
                        add_place(&mut ids, p);
                    }
                }

                InstructionValue::PropertyLoad { object, .. } => add_place(&mut ids, object),
                InstructionValue::PropertyStore { object, value, .. } => {
                    add_place(&mut ids, object);
                    add_place(&mut ids, value);
                }
                InstructionValue::PropertyDelete { object, .. } => add_place(&mut ids, object),
                InstructionValue::ComputedLoad {
                    object, property, ..
                } => {
                    add_place(&mut ids, object);
                    add_place(&mut ids, property);
                }
                InstructionValue::ComputedStore {
                    object,
                    property,
                    value,
                    ..
                } => {
                    add_place(&mut ids, object);
                    add_place(&mut ids, property);
                    add_place(&mut ids, value);
                }
                InstructionValue::ComputedDelete {
                    object, property, ..
                } => {
                    add_place(&mut ids, object);
                    add_place(&mut ids, property);
                }

                InstructionValue::StoreGlobal { value, .. } => add_place(&mut ids, value),

                InstructionValue::TaggedTemplateExpression { tag, .. } => add_place(&mut ids, tag),
                InstructionValue::TemplateLiteral { subexprs, .. } => {
                    for p in subexprs {
                        add_place(&mut ids, p);
                    }
                }

                InstructionValue::Await { value, .. } => add_place(&mut ids, value),
                InstructionValue::GetIterator { collection, .. } => add_place(&mut ids, collection),
                InstructionValue::IteratorNext {
                    iterator,
                    collection,
                    ..
                } => {
                    add_place(&mut ids, iterator);
                    add_place(&mut ids, collection);
                }
                InstructionValue::NextPropertyOf { value, .. } => add_place(&mut ids, value),

                InstructionValue::PrefixUpdate { lvalue, value, .. }
                | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
                    add_place(&mut ids, lvalue);
                    add_place(&mut ids, value);
                }

                InstructionValue::FinishMemoize { decl, .. } => add_place(&mut ids, decl),

                InstructionValue::Ternary {
                    test,
                    consequent,
                    alternate,
                    ..
                } => {
                    add_place(&mut ids, test);
                    add_place(&mut ids, consequent);
                    add_place(&mut ids, alternate);
                }

                InstructionValue::LogicalExpression { left, right, .. } => {
                    add_place(&mut ids, left);
                    add_place(&mut ids, right);
                }

                // No reads:
                InstructionValue::Primitive { .. }
                | InstructionValue::JSXText { .. }
                | InstructionValue::RegExpLiteral { .. }
                | InstructionValue::MetaProperty { .. }
                | InstructionValue::LoadGlobal { .. }
                | InstructionValue::DeclareLocal { .. }
                | InstructionValue::DeclareContext { .. }
                | InstructionValue::StartMemoize { .. }
                | InstructionValue::ObjectMethod { .. }
                | InstructionValue::FunctionExpression { .. }
                | InstructionValue::ReactiveSequenceExpression { .. }
                | InstructionValue::ReactiveOptionalExpression { .. }
                | InstructionValue::ReactiveLogicalExpression { .. }
                | InstructionValue::ReactiveConditionalExpression { .. }
                | InstructionValue::Debugger { .. } => {}
            }
        }

        // Terminal reads
        match &block.terminal {
            Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                add_place(&mut ids, value)
            }
            Terminal::If { test, .. }
            | Terminal::Branch { test, .. }
            | Terminal::Switch { test, .. } => add_place(&mut ids, test),
            _ => {}
        }

        // Phi reads
        for phi in &block.phis {
            for op in phi.operands.values() {
                add_place(&mut ids, op);
            }
        }
    }

    ids
}

/// Stops at the loop's fallthrough (which is outside the loop).
/// This ensures that blocks nested inside loop bodies (e.g., If fallthroughs
/// within a while body) are correctly marked as owned and not emitted at the
/// top-level function scope.
fn collect_loop_body_owned_blocks(
    loop_body: BlockId,
    loop_fallthrough: BlockId,
    block_map: &HashMap<BlockId, &BasicBlock>,
    owned_blocks: &mut HashSet<BlockId>,
) {
    let mut queue = vec![loop_body];
    let mut visited = HashSet::new();
    while let Some(bid) = queue.pop() {
        if !visited.insert(bid) {
            continue;
        }
        if bid == loop_fallthrough {
            continue;
        }
        owned_blocks.insert(bid);
        if let Some(block) = block_map.get(&bid) {
            for succ in terminal_successors(&block.terminal) {
                if !visited.contains(&succ) {
                    queue.push(succ);
                }
            }
        }
    }
}

/// A scope block for multi-scope codegen.
#[derive(Debug)]
struct ScopeInfo {
    range: MutableRange,
    /// Instruction IDs that belong to this scope.
    instr_ids: HashSet<InstructionId>,
}

/// Collect unique reactive scopes from all instructions in the function.
fn collect_scopes(func: &HIRFunction) -> Vec<ScopeInfo> {
    let mut seen: HashMap<ScopeId, ScopeInfo> = HashMap::new();

    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(ref scope) = instr.lvalue.identifier.scope {
                let entry = seen.entry(scope.id).or_insert_with(|| ScopeInfo {
                    range: scope.range.clone(),
                    instr_ids: HashSet::new(),
                });
                entry.instr_ids.insert(instr.id);
            }
        }
    }

    let mut scopes: Vec<ScopeInfo> = seen.into_values().collect();
    scopes.sort_by_key(|s| s.range.start.0);
    scopes
}

struct CodeGenerator {
    output: String,
    indent: usize,
    cache_slots: u32,
    /// Starting temp ID — e.g., if params use t0, codegen starts from t1.
    temp_start: usize,
    /// Map from SSA identifier ID to the expression string it represents.
    /// Used to inline temporaries.
    expr_map: HashMap<IdentifierId, String>,
    /// Map from SSA identifier ID to operator precedence level.
    /// Used to decide when to wrap inlined sub-expressions in parentheses.
    expr_precedence: HashMap<IdentifierId, u8>,
    /// Set of temporary IDs that are consumed by StoreLocal (shouldn't be emitted as standalone).
    consumed_temps: HashSet<IdentifierId>,
    /// The name of the variable that gets promoted out of the memo scope (if any).
    /// When the return value is a named variable, we promote it to `let <name>;` before the scope.
    promoted_var: Option<String>,
    /// The IdentifierId of the promoted variable's StoreLocal, so we can convert
    /// its `const`/`let` declaration into a bare assignment inside the scope.
    promoted_store_ids: HashSet<IdentifierId>,
    /// Set of identifier IDs that are JSXText values (should be rendered without {}).
    jsx_text_ids: HashSet<IdentifierId>,
    jsx_element_ids: HashSet<IdentifierId>,
    /// Set of variable names that are reassigned (should stay `let`, not promoted to `const`).
    reassigned_vars: HashSet<String>,
    /// Whether the promoted var is a "scope output" (root of a property chain),
    /// meaning there are post-scope instructions that read from the cached variable.
    is_scope_output: bool,
    /// The original return expression (when different from the promoted var).
    /// For example, when promoting `x` but returning `z` where `z = x.t`.
    post_scope_return_var: Option<String>,
    /// When true, skip memoization — emit body directly without `_c()` wrapper.
    skip_memo: bool,
    /// Name for the cache variable (default "$", changes to "$0" when "$" conflicts).
    cache_var: String,
    /// Block IDs that are loop fallthroughs — a Goto::Break targeting one of these = `break;`
    loop_fallthrough_blocks: HashSet<BlockId>,
    /// Block IDs that are loop test blocks — a Goto::Continue targeting one of these = `continue;`
    loop_test_blocks: HashSet<BlockId>,
    /// Map from unnamed temp IdentifierId → sequential name (t0, t1, t2, ...).
    /// Populated during analyze(), used by resolve_place_inner.
    temp_name_map: HashMap<IdentifierId, String>,
    /// Counter for the next temp name index.
    temp_name_counter: usize,
    /// Set of IdentifierIds that are function parameters (or LoadLocal of params).
    /// Used to skip param destructuring in codegen (pipeline handles it separately).
    param_ids: HashSet<IdentifierId>,
    /// Set of IdentifierIds for LoadLocal results of anonymous/unnamed params
    /// (params that were destructured patterns in the source, replaced by temps).
    /// Only Destructure instructions consuming these should be skipped.
    destructured_param_load_ids: HashSet<IdentifierId>,
    /// Set of IdentifierIds for StoreLocal lvalues that are part of param destructuring
    /// default chains (e.g., `[a = 2]` → the StoreLocal for `a`).
    /// Pipeline handles these, so codegen must skip them to avoid duplicates.
    param_default_stores: HashSet<IdentifierId>,
    /// Outlined functions: (name, params_str, body_str).
    outlined_functions: Vec<(String, String, String)>,
    /// Counter for outlined function names (_temp, _temp2, _temp3, ...).
    outline_counter: usize,
    /// Outer function's named identifiers (params + locals).
    /// Used to detect captures in inner function expressions for outlining.
    outer_scope_names: HashSet<String>,
    /// Map from IdentifierId → outline function name for outlined function expressions.
    outlined_map: HashMap<IdentifierId, String>,
    /// Set of IdentifierIds used as object method property values — these should NOT be outlined.
    method_property_ids: HashSet<IdentifierId>,
    /// Rename map for source variables that conflict with compiler-generated temp names.
    /// e.g., source `t0` → `t0$0` when codegen also generates `t0` as a temp.
    source_rename_map: HashMap<String, String>,
    /// Map from IdentifierId to renamed name for shadowed variables.
    /// When an inner declaration reuses a name from an outer scope (e.g., param `a`
    /// and inner `let a`), the inner one is renamed to `a_0`.
    id_rename_map: HashMap<IdentifierId, String>,
    /// Set of variable names for which `let name;` has already been emitted
    /// (e.g., promoted vars before the scope guard). Prevents duplicate declarations.
    emitted_let_decls: HashSet<String>,
    /// Pre-computed catch binding renames for inner function bodies.
    /// Maps handler_binding IdentifierId → temp name (e.g., "t1").
    /// Populated in analyze() after temp_name_map, used by generate_inner_function_body().
    catch_rename_map: HashMap<IdentifierId, String>,
    /// When true, param destructuring is NOT skipped — the body includes destructure
    /// statements for params. Used for outlined functions where the pipeline does not
    /// handle param destructuring separately.
    is_outlined: bool,
}

impl CodeGenerator {
    fn new() -> Self {
        Self {
            output: String::new(),
            indent: 1,
            cache_slots: 0,
            temp_start: 0,
            expr_map: HashMap::new(),
            expr_precedence: HashMap::new(),
            consumed_temps: HashSet::new(),
            promoted_var: None,
            promoted_store_ids: HashSet::new(),
            jsx_text_ids: HashSet::new(),
            jsx_element_ids: HashSet::new(),
            reassigned_vars: HashSet::new(),
            is_scope_output: false,
            post_scope_return_var: None,
            skip_memo: false,
            cache_var: "$".to_string(),
            loop_fallthrough_blocks: HashSet::new(),
            loop_test_blocks: HashSet::new(),
            temp_name_map: HashMap::new(),
            temp_name_counter: 0,
            param_ids: HashSet::new(),
            destructured_param_load_ids: HashSet::new(),
            param_default_stores: HashSet::new(),
            outlined_functions: Vec::new(),
            outline_counter: 0,
            outer_scope_names: HashSet::new(),
            outlined_map: HashMap::new(),
            method_property_ids: HashSet::new(),
            source_rename_map: HashMap::new(),
            id_rename_map: HashMap::new(),
            emitted_let_decls: HashSet::new(),
            catch_rename_map: HashMap::new(),
            is_outlined: false,
        }
    }

    /// Assign a sequential temp name (t0, t1, ...) to an unnamed identifier.
    fn assign_temp_name(&mut self, id: IdentifierId) {
        if !self.temp_name_map.contains_key(&id) {
            let name = format!("t{}", self.temp_name_counter);
            self.temp_name_counter += 1;
            self.temp_name_map.insert(id, name);
        }
    }

    /// Assign temp names to unnamed operands that appear inside Destructure patterns.
    fn assign_temp_names_for_operands(&mut self, instr: &Instruction) {
        if let InstructionValue::Destructure { lvalue, .. } = &instr.value {
            match &lvalue.pattern {
                Pattern::Array(arr) => {
                    for item in &arr.items {
                        if let ArrayElement::Place(p) = item
                            && p.identifier.name.is_none()
                        {
                            self.assign_temp_name(p.identifier.id);
                        }
                    }
                }
                Pattern::Object(obj) => {
                    for prop in &obj.properties {
                        match prop {
                            ObjectPropertyOrSpread::Property(p) => {
                                if p.place.identifier.name.is_none() {
                                    self.assign_temp_name(p.place.identifier.id);
                                }
                            }
                            ObjectPropertyOrSpread::Spread(p) => {
                                if p.identifier.name.is_none() {
                                    self.assign_temp_name(p.identifier.id);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// First pass: build expression map for all temporaries, track consumed temps,
    /// and identify the promoted variable (if any).
    fn analyze(&mut self, func: &HIRFunction) {
        // Collect parameter IDs so we can skip param-related Destructure in codegen
        // (pipeline handles param destructuring separately).
        // For outlined functions (is_outlined=true), we do NOT skip param destructuring
        // because the pipeline does not handle it for outlined functions — the destructuring
        // statements must be emitted as part of the function body.
        // Also track which params are anonymous/unnamed (destructured pattern params).
        let mut anon_param_ids: HashSet<IdentifierId> = HashSet::new();
        for param in &func.params {
            match param {
                Argument::Place(p) => {
                    self.param_ids.insert(p.identifier.id);
                    if p.identifier.name.is_none() {
                        anon_param_ids.insert(p.identifier.id);
                        if !self.is_outlined {
                            // Also add directly — lower_binding_pat creates Destructure
                            // instructions that use the param place directly (not via LoadLocal).
                            self.destructured_param_load_ids.insert(p.identifier.id);
                        }
                    }
                }
                Argument::Spread(p) => {
                    self.param_ids.insert(p.identifier.id);
                    if p.identifier.name.is_none() {
                        anon_param_ids.insert(p.identifier.id);
                        if !self.is_outlined {
                            self.destructured_param_load_ids.insert(p.identifier.id);
                        }
                    }
                }
            }
        }
        // Also track LoadLocal results of params (which are what Destructure consumes).
        // For anonymous params (destructured patterns), also track in destructured_param_load_ids.
        let entry_id = func.body.entry;
        for (block_id, block) in &func.body.blocks {
            if *block_id == entry_id {
                for instr in &block.instructions {
                    if let InstructionValue::LoadLocal { place, .. } = &instr.value
                        && self.param_ids.contains(&place.identifier.id)
                    {
                        self.param_ids.insert(instr.lvalue.identifier.id);
                        if !self.is_outlined && anon_param_ids.contains(&place.identifier.id) {
                            self.destructured_param_load_ids
                                .insert(instr.lvalue.identifier.id);
                        }
                    }
                }
            }
        }

        // Track param destructuring default chains so we can skip the StoreLocal
        // that HIR generates (pipeline already handles param defaults).
        // Skip for outlined functions — they need the full destructuring in the body.
        if !self.is_outlined {
            let entry_id = func.body.entry;
            let mut chain_temps: HashSet<IdentifierId> = HashSet::new();
            for (block_id, block) in &func.body.blocks {
                if *block_id != entry_id {
                    continue;
                }
                for instr in &block.instructions {
                    match &instr.value {
                        InstructionValue::Destructure { value, lvalue, .. } => {
                            if self
                                .destructured_param_load_ids
                                .contains(&value.identifier.id)
                            {
                                // Param destructure — collect unnamed pattern temps
                                match &lvalue.pattern {
                                    Pattern::Array(arr) => {
                                        for item in &arr.items {
                                            if let ArrayElement::Place(p) = item
                                                && p.identifier.name.is_none()
                                            {
                                                chain_temps.insert(p.identifier.id);
                                            }
                                        }
                                    }
                                    Pattern::Object(obj) => {
                                        for prop in &obj.properties {
                                            match prop {
                                                ObjectPropertyOrSpread::Property(p) => {
                                                    if p.place.identifier.name.is_none() {
                                                        chain_temps.insert(p.place.identifier.id);
                                                    }
                                                }
                                                ObjectPropertyOrSpread::Spread(p) => {
                                                    if p.identifier.name.is_none() {
                                                        chain_temps.insert(p.identifier.id);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        InstructionValue::StoreLocal { value, lvalue, .. } => {
                            if chain_temps.contains(&value.identifier.id) {
                                self.param_default_stores.insert(lvalue.place.identifier.id);
                            }
                        }
                        InstructionValue::BinaryExpression { left, right, .. } => {
                            if chain_temps.contains(&left.identifier.id)
                                || chain_temps.contains(&right.identifier.id)
                            {
                                chain_temps.insert(instr.lvalue.identifier.id);
                            }
                        }
                        InstructionValue::Ternary {
                            test,
                            consequent,
                            alternate,
                            ..
                        } => {
                            if chain_temps.contains(&test.identifier.id)
                                || chain_temps.contains(&consequent.identifier.id)
                                || chain_temps.contains(&alternate.identifier.id)
                            {
                                chain_temps.insert(instr.lvalue.identifier.id);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Detect naming conflicts with compiler-generated variables.
        // Collect all source variable names.
        let mut source_names: HashSet<String> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && let Some(IdentifierName::Named(n)) = &lvalue.place.identifier.name
                {
                    source_names.insert(n.clone());
                }
                if let InstructionValue::LoadLocal { place, .. } = &instr.value
                    && let Some(IdentifierName::Named(n)) = &place.identifier.name
                {
                    source_names.insert(n.clone());
                }
                // Also collect names from Destructure patterns (e.g., `let [x, setX] = ...`)
                if let InstructionValue::Destructure { lvalue, .. } = &instr.value {
                    fn collect_pattern_names(pattern: &Pattern, names: &mut HashSet<String>) {
                        match pattern {
                            Pattern::Array(arr) => {
                                for item in &arr.items {
                                    if let ArrayElement::Place(p) = item
                                        && let Some(IdentifierName::Named(n)) = &p.identifier.name
                                    {
                                        names.insert(n.clone());
                                    }
                                }
                            }
                            Pattern::Object(obj) => {
                                for prop in &obj.properties {
                                    match prop {
                                        ObjectPropertyOrSpread::Property(p) => {
                                            if let Some(IdentifierName::Named(n)) =
                                                &p.place.identifier.name
                                            {
                                                names.insert(n.clone());
                                            }
                                        }
                                        ObjectPropertyOrSpread::Spread(p) => {
                                            if let Some(IdentifierName::Named(n)) =
                                                &p.identifier.name
                                            {
                                                names.insert(n.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    collect_pattern_names(&lvalue.pattern, &mut source_names);
                }
            }
        }
        // If "$" is used as a variable, rename cache to "$0"
        if source_names.contains("$") {
            self.cache_var = "$0".to_string();
        }
        // Store outer scope names for outlining detection.
        // Also add parameter names.
        self.outer_scope_names = source_names.clone();
        for param in &func.params {
            if let Argument::Place(p) = param
                && let Some(IdentifierName::Named(n)) = &p.identifier.name
            {
                self.outer_scope_names.insert(n.clone());
            }
        }
        // Collect function identifiers used as object method property values.
        // These should NOT be outlined — they need to be emitted inline as method shorthand.
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::ObjectExpression { properties, .. } = &instr.value {
                    for prop in properties {
                        if let ObjectPropertyOrSpread::Property(p) = prop
                            && p.type_ == ObjectPropertyType::Method
                        {
                            self.method_property_ids.insert(p.place.identifier.id);
                        }
                    }
                }
            }
        }
        // Detect outlinable function expressions.
        // Skip this for already-outlined helper bodies (`is_outlined=true`):
        // those helpers are emitted standalone and do not have a follow-up emission
        // path for any nested outlines discovered here.
        if !self.is_outlined {
            // A function expression can be outlined if it doesn't capture any local variables
            // from the outer scope (only uses its own parameters, its own locals, or globals).
            // Skip functions used as object method properties — they must be emitted inline.
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    if let InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value
                    {
                        let is_method = self
                            .method_property_ids
                            .contains(&instr.lvalue.identifier.id);
                        let can_outline = self.can_outline_function(lowered_func);
                        if !is_method && can_outline {
                            let outline_name = self.next_outline_name();
                            self.outlined_map
                                .insert(instr.lvalue.identifier.id, outline_name);
                        }
                    }
                }
            }
        }

        // Detect shadowed variable names: when an inner let/const declaration reuses
        // the same name as a parameter (e.g., param `a` and inner `let a`), rename
        // the inner one to `a_0` (matching upstream renameVariables pass).
        // Only rename true shadows (new let/const declarations), NOT SSA reassignments.
        {
            // Collect parameter names — these always keep their original name
            let mut param_names: HashSet<String> = HashSet::new();
            for param in &func.params {
                if let Argument::Place(p) = param
                    && let Some(IdentifierName::Named(n)) = &p.identifier.name
                {
                    param_names.insert(n.clone());
                }
            }
            // Also include destructured param names from the entry block
            let entry_id = func.body.entry;
            for (block_id, block) in &func.body.blocks {
                if *block_id == entry_id {
                    for instr in &block.instructions {
                        if let InstructionValue::Destructure { value, lvalue, .. } = &instr.value {
                            // If the destructure source is a param, add pattern names
                            if self.param_ids.contains(&value.identifier.id) {
                                fn collect_dest_names(
                                    pattern: &Pattern,
                                    names: &mut HashSet<String>,
                                ) {
                                    match pattern {
                                        Pattern::Array(arr) => {
                                            for item in &arr.items {
                                                if let ArrayElement::Place(p) = item
                                                    && let Some(IdentifierName::Named(n)) =
                                                        &p.identifier.name
                                                {
                                                    names.insert(n.clone());
                                                }
                                            }
                                        }
                                        Pattern::Object(obj) => {
                                            for prop in &obj.properties {
                                                match prop {
                                                    ObjectPropertyOrSpread::Property(p) => {
                                                        if let Some(IdentifierName::Named(n)) =
                                                            &p.place.identifier.name
                                                        {
                                                            names.insert(n.clone());
                                                        }
                                                    }
                                                    ObjectPropertyOrSpread::Spread(p) => {
                                                        if let Some(IdentifierName::Named(n)) =
                                                            &p.identifier.name
                                                        {
                                                            names.insert(n.clone());
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                collect_dest_names(&lvalue.pattern, &mut param_names);
                            }
                        }
                    }
                }
            }
            // Scan for non-reassign StoreLocal/DeclareLocal that shadow param names
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    let decl_name_id = match &instr.value {
                        InstructionValue::StoreLocal { lvalue, .. } => {
                            // Only consider new declarations (Const/Let), not reassignments
                            if lvalue.kind != InstructionKind::Reassign {
                                match &lvalue.place.identifier.name {
                                    Some(IdentifierName::Named(n))
                                    | Some(IdentifierName::Promoted(n)) => {
                                        Some((n.clone(), lvalue.place.identifier.id))
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            }
                        }
                        InstructionValue::DeclareLocal { lvalue, .. } => {
                            match &lvalue.place.identifier.name {
                                Some(IdentifierName::Named(n))
                                | Some(IdentifierName::Promoted(n)) => {
                                    Some((n.clone(), lvalue.place.identifier.id))
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some((name, id)) = decl_name_id {
                        // Only rename if this declaration shadows a param name
                        // AND the identifier is NOT one of the param identifiers
                        if param_names.contains(&name) && !self.param_ids.contains(&id) {
                            let mut suffix = 0u32;
                            let mut new_name = format!("{}_{}", name, suffix);
                            while source_names.contains(&new_name) {
                                suffix += 1;
                                new_name = format!("{}_{}", name, suffix);
                            }
                            self.id_rename_map.insert(id, new_name.clone());
                            source_names.insert(new_name);
                        }
                    }
                }
            }
        }

        // Detect conflicts between source variable names and compiler-generated temp names.
        // Upstream renameVariables renames the SOURCE variable (e.g., source `t0` → `t0$0`).
        // We defer the actual renaming to after temp_name_map is populated (see below).

        // Collect all temporary IDs that are used as operands in any instruction.
        // These should NOT be emitted as standalone statements.
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                    if place.identifier.name.is_none() {
                        self.consumed_temps.insert(place.identifier.id);
                    }
                });
            }
            // Also check terminal operands — any temp used in a terminal
            // is consumed and should NOT be emitted as a standalone statement.
            match &block.terminal {
                Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                    if value.identifier.name.is_none() {
                        self.consumed_temps.insert(value.identifier.id);
                    }
                }
                Terminal::If { test, .. } | Terminal::Branch { test, .. } => {
                    if test.identifier.name.is_none() {
                        self.consumed_temps.insert(test.identifier.id);
                    }
                }
                Terminal::Switch { test, cases, .. } => {
                    if test.identifier.name.is_none() {
                        self.consumed_temps.insert(test.identifier.id);
                    }
                    for case in cases {
                        if let Some(t) = &case.test
                            && t.identifier.name.is_none()
                        {
                            self.consumed_temps.insert(t.identifier.id);
                        }
                    }
                }
                _ => {}
            }
            // Collect loop fallthrough and test blocks for break/continue codegen
            match &block.terminal {
                Terminal::For {
                    test, fallthrough, ..
                } => {
                    self.loop_fallthrough_blocks.insert(*fallthrough);
                    self.loop_test_blocks.insert(*test);
                }
                Terminal::ForOf {
                    test, fallthrough, ..
                } => {
                    self.loop_fallthrough_blocks.insert(*fallthrough);
                    self.loop_test_blocks.insert(*test);
                }
                Terminal::ForIn {
                    fallthrough,
                    loop_block,
                    ..
                } => {
                    self.loop_fallthrough_blocks.insert(*fallthrough);
                    // ForIn has no explicit test block; continue goes to the loop_block head
                    self.loop_test_blocks.insert(*loop_block);
                }
                Terminal::While {
                    test, fallthrough, ..
                } => {
                    self.loop_fallthrough_blocks.insert(*fallthrough);
                    self.loop_test_blocks.insert(*test);
                }
                Terminal::DoWhile {
                    test, fallthrough, ..
                } => {
                    self.loop_fallthrough_blocks.insert(*fallthrough);
                    self.loop_test_blocks.insert(*test);
                }
                _ => {}
            }
        }

        // Build expression map, track JSXText instructions, and detect reassigned variables
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                // Track JSXText identifiers
                if matches!(&instr.value, InstructionValue::JSXText { .. }) {
                    self.jsx_text_ids.insert(instr.lvalue.identifier.id);
                }
                // Track JSX element/fragment identifiers
                if matches!(
                    &instr.value,
                    InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
                ) {
                    self.jsx_element_ids.insert(instr.lvalue.identifier.id);
                }
                // Track reassigned variables
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && lvalue.kind == InstructionKind::Reassign
                    && let Some(name) = &lvalue.place.identifier.name
                {
                    match name {
                        IdentifierName::Named(n) | IdentifierName::Promoted(n) => {
                            self.reassigned_vars.insert(n.clone());
                        }
                    }
                }
                // Track variables mutated by update expressions (i++, --x, etc.)
                // The `value` field is typically a LoadLocal temp; resolve through expr_map
                match &instr.value {
                    InstructionValue::PrefixUpdate { value, .. }
                    | InstructionValue::PostfixUpdate { value, .. } => {
                        // First try the value's own name (direct named var)
                        let var_name = match &value.identifier.name {
                            Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                                Some(n.clone())
                            }
                            None => {
                                // Resolve through expr_map (temp from LoadLocal)
                                self.expr_map
                                    .get(&value.identifier.id)
                                    .filter(|s| is_valid_identifier(s))
                                    .cloned()
                            }
                        };
                        if let Some(name) = var_name {
                            self.reassigned_vars.insert(name);
                        }
                    }
                    _ => {}
                }
                let expr = self.instruction_to_expr(instr);
                if let Some(expr_str) = expr {
                    // Record the precedence level for this expression
                    let prec = instr_precedence(&instr.value);
                    if prec > 0 {
                        self.expr_precedence
                            .insert(instr.lvalue.identifier.id, prec);
                    }
                    self.expr_map.insert(instr.lvalue.identifier.id, expr_str);
                }
            }
        }

        // Build sequential temp name map (t0, t1, ...) for unnamed temporaries
        // that will appear in the output (not inlined via expr_map).
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                // Skip Destructure lvalues — their lvalue is never referenced,
                // the useful outputs are inside the pattern (handled below).
                // Also skip DeclareLocal/DeclareContext lvalues.
                let is_pattern_instr = matches!(
                    &instr.value,
                    InstructionValue::Destructure { .. }
                        | InstructionValue::DeclareLocal { .. }
                        | InstructionValue::DeclareContext { .. }
                );
                if instr.lvalue.identifier.name.is_none()
                    && !self.expr_map.contains_key(&instr.lvalue.identifier.id)
                    && !is_pattern_instr
                {
                    self.assign_temp_name(instr.lvalue.identifier.id);
                }
                // Assign names to unnamed operands inside Destructure patterns
                self.assign_temp_names_for_operands(instr);
            }
        }
        // Rename top-level catch bindings to temp names (t0, t1, ...).
        // Upstream creates catch bindings as promoted temporaries that get renamed by
        // RenameVariables. In our old codegen we handle this by assigning temp names
        // to named catch bindings at codegen time. This must run before the memo slot
        // reservation so outlined functions (which have no memo scope) get t0.
        for (_, block) in &func.body.blocks {
            if let Terminal::Try {
                handler_binding: Some(binding),
                ..
            } = &block.terminal
                && binding.identifier.name.is_some()
            {
                let temp_name = format!("t{}", self.temp_name_counter);
                self.temp_name_counter += 1;
                self.id_rename_map.insert(binding.identifier.id, temp_name);
            }
        }

        // Reserve the memo temp slot. The emit path generates `let t{temp_start};`
        // for the scope output, which doesn't go through assign_temp_name().
        // Ensure temp_name_counter is past that slot so catch bindings etc. don't collide.
        // Skip for outlined/skip_memo functions which have no memo scope.
        if !self.skip_memo && self.temp_name_counter <= self.temp_start {
            self.temp_name_counter = self.temp_start + 1;
        }

        // Post-process inner function bodies: fix catch binding temp names.
        // During instruction_to_expr, inner codegen assigned catch bindings sequential
        // names starting from 0 (e.g., t0). But for inline (non-outlined) functions,
        // the name should continue from the outer counter (e.g., t1 if outer used t0).
        // We find these and do string replacement in the expr_map.
        {
            let mut catch_fixups: Vec<(IdentifierId, String, String)> = Vec::new(); // (fn_lvalue_id, old_name, new_name)
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    let inner_func = match &instr.value {
                        InstructionValue::FunctionExpression { lowered_func, .. } => {
                            // Skip outlined functions — they have their own counter
                            if self.outlined_map.contains_key(&instr.lvalue.identifier.id) {
                                continue;
                            }
                            Some(&lowered_func.func)
                        }
                        InstructionValue::ObjectMethod { lowered_func, .. } => {
                            Some(&lowered_func.func)
                        }
                        _ => None,
                    };
                    if let Some(inner_fn) = inner_func {
                        // Count how many catch bindings the inner codegen would have renamed
                        // (they get t0, t1, ... from the inner counter starting at 0).
                        let mut inner_counter = 0usize;
                        for (_, inner_block) in &inner_fn.body.blocks {
                            if let Terminal::Try {
                                handler_binding: Some(binding),
                                ..
                            } = &inner_block.terminal
                                && binding.identifier.name.is_some()
                            {
                                let old_name = format!("t{}", inner_counter);
                                let new_name = format!("t{}", self.temp_name_counter);
                                self.temp_name_counter += 1;
                                catch_fixups.push((
                                    instr.lvalue.identifier.id,
                                    old_name,
                                    new_name.clone(),
                                ));
                                self.catch_rename_map
                                    .insert(binding.identifier.id, new_name);
                                inner_counter += 1;
                            }
                        }
                    }
                }
            }
            // Apply fixups: replace catch binding names in ALL expr_map entries.
            // The function body string may have been inlined into parent expressions
            // (e.g., ObjectExpression consuming the FunctionExpression via resolve_place).
            for (_fn_id, old_name, new_name) in &catch_fixups {
                let old_catch = format!("catch ({}) {{", old_name);
                let new_catch = format!("catch ({}) {{", new_name);
                for expr in self.expr_map.values_mut() {
                    if expr.contains(&old_catch) {
                        *expr = expr.replace(&old_catch, &new_catch);
                    }
                }
            }
        }

        // Fix up expr_map: replace _t{id} references with the assigned tN names
        if !self.temp_name_map.is_empty() {
            let replacements: Vec<(String, String)> = self
                .temp_name_map
                .iter()
                .map(|(id, name)| (format!("_t{}", id.0), name.clone()))
                .collect();
            for expr in self.expr_map.values_mut() {
                for (old, new) in &replacements {
                    if expr.contains(old.as_str()) {
                        *expr = expr.replace(old.as_str(), new.as_str());
                    }
                }
            }
        }

        // Detect promoted variable: if the return value is a named local variable
        // that is referenced by other instructions (not just assigned and returned),
        // promote it so the memo scope uses that name instead of `t0`.
        //
        // Heuristics (in priority order):
        // 1. If the return value is derived from a variable (PropertyLoad chain),
        //    promote the root variable (e.g., return z where z = x.t → promote x)
        // 2. If the return value is a named variable with LoadLocal count > 1
        //    (i.e., used somewhere beyond the return), promote it
        for (_, block) in &func.body.blocks {
            if let Terminal::Return {
                value,
                return_variant,
                ..
            } = &block.terminal
                && (*return_variant == ReturnVariant::Explicit
                    || *return_variant == ReturnVariant::Implicit)
            {
                // First: try to find the root variable via property chain
                if let Some(root_name) = self.find_scope_output_var(func, value) {
                    let return_name = self.get_named_var(value);
                    // If the root is different from the return var, this is a
                    // scope output pattern (post-scope instructions needed)
                    if return_name.as_deref() != Some(&root_name) {
                        self.is_scope_output = true;
                        self.post_scope_return_var = return_name;
                    }
                    self.promoted_var = Some(root_name.clone());
                    self.find_promoted_stores(func, &root_name);
                } else if self.is_return_place_from_load_global(func, value) {
                    // Returning a direct LoadGlobal alias (e.g. outlined helper name)
                    // should stay as `return <global>;` and must not trigger
                    // scope-output promotion heuristics.
                } else if let Some(name) = self.get_named_var(value) {
                    let load_count = self.count_loads_of_var(func, &name);
                    if load_count > 1 {
                        self.promoted_var = Some(name.clone());
                        self.find_promoted_stores(func, &name);
                    } else {
                        // Return value is a named variable with only 1 load (the return itself).
                        // Use scope output pattern: temp inside scope, named var after scope.
                        self.is_scope_output = true;
                        self.post_scope_return_var = Some(name.clone());
                    }
                }
            }
        }

        // Detect conflicts between source variable names and generated temp names.
        // Must happen after promoted_var detection since the memo var name depends on it.
        {
            let mut generated_names: HashSet<String> =
                self.temp_name_map.values().cloned().collect();
            // The memo var is `t{temp_start}` when there's no promoted_var
            let memo_temp = format!("t{}", self.temp_start);
            if self.promoted_var.is_none() {
                generated_names.insert(memo_temp.clone());
            }
            for src_name in &source_names {
                if generated_names.contains(src_name) {
                    self.source_rename_map
                        .insert(src_name.clone(), format!("{}_0", src_name));
                }
            }
            // If the promoted_var is a source variable that matches a compiler
            // temp pattern (t0, t1, etc.), rename it and un-promote.
            // The upstream renameVariables pass always renames such variables.
            // Pattern: promoted_var "t0" → un-promote, rename source t0 → t0$0,
            // use raw temp t0 for scope, post-scope: const t0$0 = t0; return t0$0;
            if let Some(pv) = self.promoted_var.clone() {
                // Check if promoted_var matches temp pattern AND is a real source name
                let is_temp_pattern = pv.starts_with('t')
                    && pv.len() > 1
                    && pv[1..].chars().all(|c| c.is_ascii_digit());
                let has_rename = self.source_rename_map.contains_key(&pv);
                if is_temp_pattern && !has_rename && source_names.contains(&pv) {
                    // Add rename for this source var
                    self.source_rename_map
                        .insert(pv.clone(), format!("{}_0", pv));
                    // Fix up expr_map
                    let old = pv.clone();
                    let new_name = format!("{}_0", pv);
                    for expr in self.expr_map.values_mut() {
                        *expr = replace_whole_word(expr, &old, &new_name);
                    }
                }
                if let Some(renamed) = self.source_rename_map.get(&pv).cloned() {
                    // Un-promote: use raw temp, add post-scope link
                    self.promoted_var = None;
                    self.promoted_store_ids.clear();
                    self.is_scope_output = true;
                    self.post_scope_return_var = Some(renamed);
                }
            }
            // Also handle: post_scope_return_var that matches a temp pattern.
            // When is_scope_output=true and post_scope_return_var is "t0",
            // the source var needs to be renamed to "t0$0".
            if let Some(psrv) = self.post_scope_return_var.clone() {
                let is_temp_pattern = psrv.starts_with('t')
                    && psrv.len() > 1
                    && psrv[1..].chars().all(|c| c.is_ascii_digit());
                if is_temp_pattern
                    && source_names.contains(&psrv)
                    && !self.source_rename_map.contains_key(&psrv)
                {
                    let renamed = format!("{}_0", psrv);
                    self.source_rename_map.insert(psrv, renamed.clone());
                    self.post_scope_return_var = Some(renamed);
                }
                // If already renamed via source_rename_map, update post_scope_return_var
                if let Some(renamed) = self
                    .source_rename_map
                    .get(self.post_scope_return_var.as_deref().unwrap_or(""))
                    .cloned()
                {
                    self.post_scope_return_var = Some(renamed);
                }
            }
            // Fix up expr_map: rename source variable references
            if !self.source_rename_map.is_empty() {
                for expr in self.expr_map.values_mut() {
                    for (old, new) in &self.source_rename_map {
                        *expr = replace_whole_word(expr, old, new);
                    }
                }
            }
        }

        // Generate bodies for outlined functions (now that expr_map is complete).
        if !self.outlined_map.is_empty() {
            let outlined_ids: Vec<(IdentifierId, String)> = self
                .outlined_map
                .iter()
                .map(|(id, name)| (*id, name.clone()))
                .collect();
            for (id, outline_name) in outlined_ids {
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        if instr.lvalue.identifier.id == id {
                            if let InstructionValue::FunctionExpression { lowered_func, .. } =
                                &instr.value
                            {
                                let inner_body =
                                    self.generate_inner_function_body(&lowered_func.func);
                                let params =
                                    self.generate_inner_function_params(&lowered_func.func);
                                self.outlined_functions.push((
                                    outline_name.clone(),
                                    params,
                                    inner_body,
                                ));
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Get the name of a place if it resolves to a named variable (not a temporary).
    fn get_named_var(&self, place: &Place) -> Option<String> {
        // First check if it's a LoadLocal that refers to a named var
        if let Some(expr) = self.expr_map.get(&place.identifier.id) {
            // If the expr is just a variable name (not a numeric literal),
            // it means this temp just loads a named variable.
            if is_valid_identifier(expr) {
                return Some(expr.clone());
            }
        }
        // Direct named place
        match &place.identifier.name {
            Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
                Some(name.clone())
            }
            None => None,
        }
    }

    /// Count how many LoadLocal instructions reference a named variable.
    /// Used to determine if the variable is used beyond just the return statement.
    fn count_loads_of_var(&self, func: &HIRFunction, var_name: &str) -> usize {
        let mut count = 0;
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::LoadLocal { place, .. } = &instr.value {
                    let name = match &place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => continue,
                    };
                    if name == var_name {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    /// Find the "scope output" variable: the root mutable variable that the return value
    /// is derived from. For example, if the function returns `z` where `z = x.t`,
    /// and `x` is created and mutated in the scope, `x` is the scope output.
    ///
    /// Returns Some(var_name) if a root variable is found that:
    /// - Is not the direct return variable (that case is handled by the LoadLocal count heuristic)
    /// - Is assigned with Let/Const kind AND is also mutated (PropertyStore/ComputedStore/MethodCall)
    fn find_scope_output_var(&self, func: &HIRFunction, return_place: &Place) -> Option<String> {
        // Trace the return value back to find what variable it depends on
        let return_name = self.get_named_var(return_place)?;

        // Find the StoreLocal for the return variable and see what expression it stores
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    let name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => continue,
                    };
                    if name == return_name {
                        // Check what expression was stored — look in expr_map for the value
                        if let Some(expr) = self.expr_map.get(&value.identifier.id) {
                            // If the expression is a PropertyLoad like `x.t`, extract `x`
                            if let Some(dot_idx) = expr.find('.') {
                                let root = &expr[..dot_idx];
                                if is_valid_identifier(root)
                                    && (self.is_mutated_var(func, root)
                                        || self.is_allocating_var(func, root))
                                {
                                    return Some(root.to_string());
                                }
                            }
                            // If the expression is a ComputedLoad like `x[0]`, extract `x`
                            if let Some(bracket_idx) = expr.find('[') {
                                let root = &expr[..bracket_idx];
                                if is_valid_identifier(root)
                                    && (self.is_mutated_var(func, root)
                                        || self.is_allocating_var(func, root))
                                {
                                    return Some(root.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }

        // Also check: if the return variable itself is created AND mutated, promote it
        // even if LoadLocal count is only 1 (e.g., `let x = {}; x.t = q; return x;`)
        if self.is_mutated_var(func, &return_name) {
            return Some(return_name);
        }

        // Also check: if the return variable itself is allocated (object/array/function/JSX)
        if self.is_allocating_var(func, &return_name) {
            return Some(return_name);
        }

        None
    }

    /// Check if a named variable is mutated (PropertyStore, ComputedStore, or MethodCall on it).
    fn is_mutated_var(&self, func: &HIRFunction, var_name: &str) -> bool {
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::PropertyStore { object, .. }
                    | InstructionValue::ComputedStore { object, .. } => {
                        if self.resolve_to_name(object) == Some(var_name.to_string()) {
                            return true;
                        }
                    }
                    InstructionValue::MethodCall { receiver, .. } => {
                        if self.resolve_to_name(receiver) == Some(var_name.to_string()) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// Check if a named variable is assigned an allocating expression (ObjectExpression,
    /// ArrayExpression, JsxExpression, FunctionExpression, NewExpression). These create
    /// new heap values that benefit from memoization.
    fn is_allocating_var(&self, func: &HIRFunction, var_name: &str) -> bool {
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    let name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => continue,
                    };
                    if name == var_name {
                        // Check if the value points to an allocating expression
                        if let Some(src_instr) =
                            self.find_instruction_for_id(func, value.identifier.id)
                        {
                            return matches!(
                                src_instr.value,
                                InstructionValue::ObjectExpression { .. }
                                    | InstructionValue::ArrayExpression { .. }
                                    | InstructionValue::JsxExpression { .. }
                                    | InstructionValue::JsxFragment { .. }
                                    | InstructionValue::FunctionExpression { .. }
                                    | InstructionValue::NewExpression { .. }
                            );
                        }
                    }
                }
            }
        }
        false
    }

    /// Find the instruction that produces a given identifier ID.
    fn find_instruction_for_id<'b>(
        &self,
        func: &'b HIRFunction,
        id: IdentifierId,
    ) -> Option<&'b Instruction> {
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if instr.lvalue.identifier.id == id {
                    return Some(instr);
                }
            }
        }
        None
    }

    fn is_return_place_from_load_global(&self, func: &HIRFunction, place: &Place) -> bool {
        self.find_instruction_for_id(func, place.identifier.id)
            .is_some_and(|instr| matches!(&instr.value, InstructionValue::LoadGlobal { .. }))
    }

    /// Resolve a place to its variable name (through LoadLocal / expr_map).
    fn resolve_to_name(&self, place: &Place) -> Option<String> {
        if let Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) =
            &place.identifier.name
        {
            return Some(n.clone());
        }
        if let Some(expr) = self.expr_map.get(&place.identifier.id)
            && is_valid_identifier(expr)
        {
            return Some(expr.clone());
        }
        None
    }

    /// Find StoreLocal instructions that store into the promoted variable.
    fn find_promoted_stores(&mut self, func: &HIRFunction, promoted_name: &str) {
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                    let name = match &lvalue.place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            n.as_str()
                        }
                        None => continue,
                    };
                    if name == promoted_name {
                        self.promoted_store_ids.insert(lvalue.place.identifier.id);
                    }
                }
            }
        }
    }

    /// Determine the scope for a terminal's generated statement by looking at
    /// scope annotations on instructions within the terminal's sub-blocks.
    fn find_scope_in_block_instructions(
        func: &HIRFunction,
        terminal: &Terminal,
        scope_infos: &[ScopeCodegenInfo],
    ) -> Option<ScopeId> {
        // Collect all block IDs relevant to this terminal
        let block_ids: Vec<BlockId> = match terminal {
            Terminal::For {
                init,
                test,
                update,
                loop_block,
                ..
            } => {
                let mut ids = vec![*init, *test, *loop_block];
                if let Some(u) = update {
                    ids.push(*u);
                }
                ids
            }
            Terminal::ForOf {
                init,
                test,
                loop_block,
                ..
            } => vec![*init, *test, *loop_block],
            Terminal::ForIn {
                init, loop_block, ..
            } => vec![*init, *loop_block],
            Terminal::While {
                test, loop_block, ..
            } => vec![*test, *loop_block],
            Terminal::DoWhile {
                loop_block, test, ..
            } => vec![*loop_block, *test],
            Terminal::If {
                consequent,
                alternate,
                ..
            }
            | Terminal::Branch {
                consequent,
                alternate,
                ..
            } => vec![*consequent, *alternate],
            Terminal::Switch { cases, .. } => cases.iter().map(|c| c.block).collect(),
            Terminal::Try { block, handler, .. } => vec![*block, *handler],
            Terminal::Throw { .. } => vec![],
            _ => return None,
        };

        // Find the most common scope among instructions in these blocks
        for (_, block) in &func.body.blocks {
            if block_ids.contains(&block.id) {
                for instr in &block.instructions {
                    if let Some(scope) = &instr.lvalue.identifier.scope {
                        // Verify this scope is one of the surviving scopes
                        if scope_infos.iter().any(|s| s.id == scope.id) {
                            return Some(scope.id);
                        }
                    }
                }
            }
        }
        None
    }

    /// Multi-scope codegen: emits multiple if-blocks, one per reactive scope.
    /// Each scope gets its own cache slots and dep checks.
    fn emit_function_multi_scope(&mut self, func: &HIRFunction) {
        let scope_infos = collect_scope_codegen_info(func, self);
        if scope_infos.is_empty() {
            // Fallback to single-scope
            return self.emit_function(func);
        }

        // Build block map and owned blocks (reuse from emit_function)
        let block_map: HashMap<BlockId, &BasicBlock> = func
            .body
            .blocks
            .iter()
            .map(|(id, block)| (*id, block))
            .collect();

        let mut owned_blocks: HashSet<BlockId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    if let Some(u) = update {
                        owned_blocks.insert(*u);
                    }
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForOf {
                    init,
                    test,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    owned_blocks.insert(*loop_block);
                    owned_blocks.insert(*test);
                }
                Terminal::If {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    owned_blocks.insert(*consequent);
                    if *alternate != *fallthrough {
                        owned_blocks.insert(*alternate);
                    }
                }
                Terminal::Try {
                    block: try_block,
                    handler,
                    ..
                } => {
                    owned_blocks.insert(*try_block);
                    owned_blocks.insert(*handler);
                }
                Terminal::Switch { cases, .. } => {
                    for case in cases {
                        owned_blocks.insert(case.block);
                    }
                }
                _ => {}
            }
        }

        // Transitively own loop body blocks
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForOf {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForIn {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::While {
                    test,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::DoWhile {
                    loop_block,
                    test,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                _ => {}
            }
        }

        // Collect tagged statements: (scope_id, stmt)
        // Also collect hook calls and return expression separately
        struct TaggedStmt {
            scope_id: Option<ScopeId>,
            stmt: String,
            is_hook: bool,
        }

        let mut tagged_stmts: Vec<TaggedStmt> = Vec::new();
        let mut return_expr: Option<String> = None;

        for (_, block) in &func.body.blocks {
            if owned_blocks.contains(&block.id) {
                continue;
            }

            for instr in &block.instructions {
                if let Some(stmt) = self.instruction_to_stmt(instr) {
                    // Use the instruction's own scope annotation rather than range-based lookup
                    let scope_id = instr.lvalue.identifier.scope.as_ref().map(|s| s.id);
                    let is_hook = self.is_hook_call_stmt(instr);
                    tagged_stmts.push(TaggedStmt {
                        scope_id,
                        stmt,
                        is_hook,
                    });
                }
            }

            // Handle terminals that produce statements
            // For terminals, determine scope from the instructions within the terminal's blocks
            let terminal_scope =
                Self::find_scope_in_block_instructions(func, &block.terminal, &scope_infos);
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    let init_s = self.get_for_init_str(*init, &block_map);
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let update_expr = update
                        .map(|u| self.get_for_update_expr(u, &block_map))
                        .unwrap_or_default();
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str =
                        format!("for ({}; {}; {}) {{", init_s, test_expr, update_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: loop_str,
                        is_hook: false,
                    });
                }
                Terminal::ForOf {
                    init, loop_block, ..
                } => {
                    let init_expr = self.get_block_expr(*init, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        self.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else if !var_decl.is_empty() {
                            Vec::new()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} of {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: loop_str,
                        is_hook: false,
                    });
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    let init_expr = self.get_block_expr(*init, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        self.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else if !var_decl.is_empty() {
                            Vec::new()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} in {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: loop_str,
                        is_hook: false,
                    });
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = format!("while ({}) {{", test_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: loop_str,
                        is_hook: false,
                    });
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = "do {".to_string();
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str(&format!("\n}} while ({});", test_expr));
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: loop_str,
                        is_hook: false,
                    });
                }
                Terminal::If {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    let if_str = self.emit_if_terminal(
                        test,
                        *consequent,
                        *alternate,
                        *fallthrough,
                        &block_map,
                        &owned_blocks,
                    );
                    if let Some(s) = if_str {
                        tagged_stmts.push(TaggedStmt {
                            scope_id: terminal_scope,
                            stmt: s,
                            is_hook: false,
                        });
                    }
                }
                Terminal::Switch { test, cases, .. } => {
                    let test_expr = self.resolve_place(test);
                    let mut switch_str = format!("switch ({}) {{", test_expr);
                    for case in cases {
                        let case_stmts =
                            self.collect_loop_body_stmts(case.block, &block_map, &owned_blocks);
                        if let Some(test_place) = &case.test {
                            let case_test = self.resolve_place(test_place);
                            switch_str.push_str(&format!("\n  case {}:", case_test));
                        } else {
                            switch_str.push_str("\n  default:");
                        }
                        for s in &case_stmts {
                            switch_str.push_str(&format!("\n    {}", s));
                        }
                    }
                    switch_str.push_str("\n}");
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: switch_str,
                        is_hook: false,
                    });
                }
                Terminal::Throw { value, .. } => {
                    let val = self.resolve_place(value);
                    tagged_stmts.push(TaggedStmt {
                        scope_id: terminal_scope,
                        stmt: format!("throw {};", val),
                        is_hook: false,
                    });
                }
                Terminal::Return {
                    value,
                    return_variant,
                    ..
                } => {
                    if *return_variant == ReturnVariant::Explicit
                        || *return_variant == ReturnVariant::Implicit
                    {
                        return_expr = Some(self.resolve_place(value));
                    }
                }
                _ => {}
            }
        }

        // Build segments: group consecutive stmts by scope_id
        enum Segment {
            Hook(String),
            Plain(Vec<String>),
            Scope {
                scope_idx: usize,
                stmts: Vec<String>,
            },
        }

        let mut segments: Vec<Segment> = Vec::new();
        let mut current_scope: Option<ScopeId> = None;
        let mut current_stmts: Vec<String> = Vec::new();
        let mut current_is_plain = true;

        for tagged in &tagged_stmts {
            // Hook calls always go to pre-scope
            if tagged.is_hook {
                // Flush current
                if !current_stmts.is_empty() {
                    if current_is_plain {
                        segments.push(Segment::Plain(std::mem::take(&mut current_stmts)));
                    } else if let Some(sid) = current_scope {
                        let idx = scope_infos.iter().position(|s| s.id == sid).unwrap_or(0);
                        segments.push(Segment::Scope {
                            scope_idx: idx,
                            stmts: std::mem::take(&mut current_stmts),
                        });
                    }
                    current_scope = None;
                    current_is_plain = true;
                }
                segments.push(Segment::Hook(tagged.stmt.clone()));
                continue;
            }

            let ts = tagged.scope_id;
            if ts != current_scope || (ts.is_none() != current_is_plain) {
                // Flush current
                if !current_stmts.is_empty() {
                    if current_is_plain {
                        segments.push(Segment::Plain(std::mem::take(&mut current_stmts)));
                    } else if let Some(sid) = current_scope {
                        let idx = scope_infos.iter().position(|s| s.id == sid).unwrap_or(0);
                        segments.push(Segment::Scope {
                            scope_idx: idx,
                            stmts: std::mem::take(&mut current_stmts),
                        });
                    }
                }
                current_scope = ts;
                current_is_plain = ts.is_none();
            }
            current_stmts.push(tagged.stmt.clone());
        }
        // Flush remaining
        if !current_stmts.is_empty() {
            if current_is_plain {
                segments.push(Segment::Plain(current_stmts));
            } else if let Some(sid) = current_scope {
                let idx = scope_infos.iter().position(|s| s.id == sid).unwrap_or(0);
                segments.push(Segment::Scope {
                    scope_idx: idx,
                    stmts: current_stmts,
                });
            }
        }

        // Calculate total cache slots
        let mut total_slots: u32 = 0;
        let mut scope_slot_offsets: Vec<u32> = Vec::new();
        for scope in &scope_infos {
            scope_slot_offsets.push(total_slots);
            let n_deps = scope.deps.len() as u32;
            let n_decls = if scope.decl_names.is_empty() {
                1u32
            } else {
                scope.decl_names.len() as u32
            };
            total_slots += n_deps + n_decls;
        }

        if total_slots == 0 {
            // No scopes need cache — emit plain
            for seg in &segments {
                match seg {
                    Segment::Hook(s) => {
                        self.emit_line(s);
                    }
                    Segment::Plain(stmts) => {
                        for s in stmts {
                            self.emit_line(s);
                        }
                    }
                    Segment::Scope { stmts, .. } => {
                        for s in stmts {
                            self.emit_line(s);
                        }
                    }
                }
            }
            if let Some(ref ret) = return_expr
                && ret != "undefined"
            {
                self.emit_line(&format!("return {};", ret));
            }
            return;
        }

        // Emit cache allocation
        let cv = self.cache_var.clone();
        self.cache_slots = total_slots;
        self.emit_line(&format!("const {} = _c({});", cv, total_slots));

        // Track which scope declarations have been emitted
        let mut emitted_decls: HashSet<String> = HashSet::new();

        // Emit segments
        for seg in &segments {
            match seg {
                Segment::Hook(s) => {
                    self.emit_line(s);
                }
                Segment::Plain(stmts) => {
                    for s in stmts {
                        self.emit_line(s);
                    }
                }
                Segment::Scope { scope_idx, stmts } => {
                    let scope = &scope_infos[*scope_idx];
                    let slot_start = scope_slot_offsets[*scope_idx];
                    let deps = &scope.deps;
                    let decl_names = &scope.decl_names;
                    let n_deps = deps.len() as u32;

                    // Use declaration names or generate temp names
                    let output_names: Vec<String> = if decl_names.is_empty() {
                        // No named declarations — use a temp var
                        let temp = format!("t{}", self.temp_name_counter);
                        self.temp_name_counter += 1;
                        vec![temp]
                    } else {
                        decl_names.clone()
                    };

                    // Emit declarations before if-block
                    for name in &output_names {
                        if !emitted_decls.contains(name) && !self.emitted_let_decls.contains(name) {
                            self.emit_line(&format!("let {};", name));
                            emitted_decls.insert(name.clone());
                        }
                    }

                    // Emit if-condition
                    if deps.is_empty() {
                        self.emit_line(&format!(
                            "if ({}[{}] === Symbol.for(\"react.memo_cache_sentinel\")) {{",
                            cv, slot_start
                        ));
                    } else {
                        let checks: Vec<String> = deps
                            .iter()
                            .enumerate()
                            .map(|(i, d)| format!("{}[{}] !== {}", cv, slot_start + i as u32, d))
                            .collect();
                        self.emit_line(&format!("if ({}) {{", checks.join(" || ")));
                    }
                    self.indent += 1;

                    // Emit scope body
                    for s in stmts {
                        // Skip duplicate declarations for output vars
                        let is_dup_decl = output_names.iter().any(|n| s == &format!("let {};", n));
                        if is_dup_decl {
                            continue;
                        }
                        self.emit_line(s);
                    }

                    // Store deps to cache
                    for (i, d) in deps.iter().enumerate() {
                        self.emit_line(&format!("{}[{}] = {};", cv, slot_start + i as u32, d));
                    }

                    // Store declarations to cache
                    for (i, name) in output_names.iter().enumerate() {
                        self.emit_line(&format!(
                            "{}[{}] = {};",
                            cv,
                            slot_start + n_deps + i as u32,
                            name
                        ));
                    }

                    self.indent -= 1;
                    self.emit_line("} else {");
                    self.indent += 1;

                    // Load declarations from cache
                    for (i, name) in output_names.iter().enumerate() {
                        self.emit_line(&format!(
                            "{} = {}[{}];",
                            name,
                            cv,
                            slot_start + n_deps + i as u32
                        ));
                    }

                    self.indent -= 1;
                    self.emit_line("}");
                }
            }
        }

        // Emit return
        if let Some(ref ret) = return_expr
            && ret != "undefined"
        {
            self.emit_line(&format!("return {};", ret));
        }
    }

    fn emit_function(&mut self, func: &HIRFunction) {
        let scope_codegen_infos = collect_scope_codegen_info(func, self);
        // Outlined/no-memo codegen must never route through multi-scope memo emission,
        // which always allocates cache slots and emits _c(...) guards.
        if !self.skip_memo && scope_codegen_infos.len() > 1 {
            self.emit_function_multi_scope(func);
            return;
        }

        // Collect surviving scope ranges for scope-boundary classification.
        // Instructions with IDs within a scope range go in the memo body;
        // instructions with IDs after ALL scope ranges go in post-scope.
        let scope_infos = collect_scopes(func);
        let scope_range: Option<(InstructionId, InstructionId)> = if scope_infos.is_empty() {
            None
        } else {
            // Compute the union range of all surviving scopes
            let start = scope_infos.iter().map(|s| s.range.start).min().unwrap();
            let end = scope_infos.iter().map(|s| s.range.end).max().unwrap();
            Some((start, end))
        };

        // Collect statements and return value from all blocks
        // Separate into: pre-memo (hook calls), non-scope (outside reactive scopes),
        // memo body (inside reactive scope), post-scope, and return
        let mut pre_memo_stmts: Vec<String> = Vec::new();
        let mut memo_stmts: Vec<String> = Vec::new();
        let mut post_scope_stmts: Vec<String> = Vec::new();
        // Track which instructions are classified as pre-scope (hook calls, outlined).
        // Everything NOT in this set is considered part of the memo scope.
        // Used to filter deps to only those referenced by memo-scope instructions.
        let mut pre_scope_instr_ids: HashSet<InstructionId> = HashSet::new();
        // Track statements eligible for side-effect hoisting (moved AFTER needs_memo check)
        let mut side_effect_hoistable: HashSet<String> = HashSet::new();
        // All statements in source order (used when needs_memo is false to avoid
        // reordering caused by hook-call separation)
        let mut all_stmts_in_order: Vec<String> = Vec::new();
        let mut return_expr: Option<String> = None;

        // When is_scope_output is true, instructions that read from the promoted
        // variable should go AFTER the scope, not inside it.
        let promoted_var_name = self.promoted_var.clone();

        // When is_scope_output && promoted_var is None, the return var's StoreLocal
        // should be demoted to a temp assignment inside the scope, with the named
        // var assigned from the temp after the scope.
        let demoted_return_var = if self.is_scope_output && self.promoted_var.is_none() {
            self.post_scope_return_var.clone()
        } else {
            None
        };
        let memo_var_name = self
            .promoted_var
            .clone()
            .unwrap_or_else(|| format!("t{}", self.temp_start));

        // Build block lookup map
        let block_map: HashMap<BlockId, &BasicBlock> = func
            .body
            .blocks
            .iter()
            .map(|(id, block)| (*id, block))
            .collect();

        // Build set of blocks that are "owned" by loop/if terminals (should not be emitted linearly)
        let mut owned_blocks: HashSet<BlockId> = HashSet::new();
        // Track top-level try-catch wrapper: when the function body starts with a Try terminal,
        // we don't own the try block — instead we process it linearly and wrap the output.
        let mut try_wrapper: Option<(Option<Place>, BlockId)> = None; // (handler_binding, handler_block)
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    if let Some(u) = update {
                        owned_blocks.insert(*u);
                    }
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForOf {
                    init,
                    test,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    owned_blocks.insert(*loop_block);
                    owned_blocks.insert(*test);
                }
                Terminal::If {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    owned_blocks.insert(*consequent);
                    // Only own the alternate block if it's different from fallthrough.
                    // When alternate == fallthrough, there's no else branch — the
                    // fallthrough block has the code after the if and should be
                    // processed linearly, not owned by the if terminal.
                    if *alternate != *fallthrough {
                        owned_blocks.insert(*alternate);
                    }
                }
                Terminal::Try {
                    block: try_block,
                    handler,
                    handler_binding,
                    ..
                } => {
                    // Check if this is a top-level try-catch (the block with Try terminal
                    // is not owned by any other terminal). For top-level try, we don't own
                    // the try block — we process it inline and wrap the output.
                    // Skip empty try blocks (pruneMaybeThrows equivalent) — if the try
                    // block has no instructions and just goes to fallthrough, eliminate
                    // the try-catch entirely.
                    let try_block_empty = block_map.get(try_block).is_some_and(|b| {
                        b.instructions.is_empty() && matches!(b.terminal, Terminal::Goto { .. })
                    });
                    if try_block_empty {
                        // Empty try block — skip the try-catch entirely, treat both
                        // try block and handler as dead code
                        owned_blocks.insert(*try_block);
                        owned_blocks.insert(*handler);
                    } else if !owned_blocks.contains(&block.id) && try_wrapper.is_none() {
                        // Top-level try: don't own the try block, just own the handler
                        try_wrapper = Some((handler_binding.clone(), *handler));
                        owned_blocks.insert(*handler);
                    } else {
                        // Nested try: own both blocks
                        owned_blocks.insert(*try_block);
                        owned_blocks.insert(*handler);
                    }
                }
                Terminal::Switch { cases, .. } => {
                    for case in cases {
                        owned_blocks.insert(case.block);
                    }
                }
                _ => {}
            }
        }

        // Transitively own all blocks reachable within loop bodies.
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForOf {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForIn {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::While {
                    test,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::DoWhile {
                    loop_block,
                    test,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                _ => {}
            }
        }

        // Pre-scan: build set of identifier IDs produced by LoadGlobal instructions.
        // This allows us to identify fire-and-forget calls to global receivers (e.g., console.log)
        // vs local function calls (e.g., x()) which may mutate scoped values.
        let mut global_ids: HashSet<IdentifierId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if matches!(&instr.value, InstructionValue::LoadGlobal { .. }) {
                    global_ids.insert(instr.lvalue.identifier.id);
                }
            }
        }

        // Build consumed_ids: identifiers that are read by any instruction.
        // Unused results (lvalue ids NOT in this set) correspond to fire-and-forget calls.
        let consumed_ids = build_consumed_ids(func);

        // Build param_ids for prescope argument detection
        let param_ids: HashSet<IdentifierId> = func
            .params
            .iter()
            .map(|p| match p {
                Argument::Place(p) | Argument::Spread(p) => p.identifier.id,
            })
            .collect();

        for (_, block) in &func.body.blocks {
            // Skip blocks that are owned by loop terminals
            if owned_blocks.contains(&block.id) {
                continue;
            }

            for instr in &block.instructions {
                // Check if this StoreLocal should be demoted to a temp assignment
                let mut demoted = false;
                if let Some(ref dvar) = demoted_return_var
                    && let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                {
                    let name = self.identifier_name(&lvalue.place.identifier);
                    if name == *dvar {
                        // Convert `const x = expr;` to `memo_var = expr;`
                        let val = self.resolve_place(value);
                        memo_stmts.push(format!("{} = {};", memo_var_name, val));
                        demoted = true;
                    }
                }
                if demoted {
                    continue;
                }

                if let Some(stmt) = self.instruction_to_stmt(instr) {
                    // Always record source order for non-memo fallback
                    all_stmts_in_order.push(stmt.clone());
                    // When skip_memo is true (all scopes pruned), don't separate
                    // hook calls — emit everything in source order.
                    if self.skip_memo {
                        memo_stmts.push(stmt);
                    } else if (self.is_scope_output
                        && self.is_post_scope_instruction(instr, promoted_var_name.as_deref()))
                        || (self.promoted_var.is_some()
                            && self
                                .is_readonly_post_scope_call(instr, promoted_var_name.as_deref()))
                    {
                        post_scope_stmts.push(stmt);
                        pre_scope_instr_ids.insert(instr.id);
                    } else if self.is_hook_call_stmt(instr) || self.is_outlined_store(instr) {
                        pre_memo_stmts.push(stmt);
                        pre_scope_instr_ids.insert(instr.id);
                    } else if self.is_pre_scope_instruction(
                        instr,
                        &scope_range,
                        &pre_scope_instr_ids,
                        &param_ids,
                        &global_ids,
                    ) {
                        // Non-hook instructions before the scope range that only use
                        // pre-scope values → place before the scope guard
                        pre_memo_stmts.push(stmt);
                        pre_scope_instr_ids.insert(instr.id);
                    } else {
                        // Tag side-effect-only calls for potential hoisting later
                        if self.is_side_effect_only_call(instr, &global_ids)
                            && !consumed_ids.contains(&instr.lvalue.identifier.id)
                            && self.call_args_are_prescope(instr, &param_ids, &global_ids)
                        {
                            side_effect_hoistable.insert(stmt.clone());
                        }
                        memo_stmts.push(stmt);
                    }
                }
            }

            // Handle loop terminals: emit loop structures
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    let init_s = self.get_for_init_str(*init, &block_map);
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let update_expr = update
                        .map(|u| self.get_for_update_expr(u, &block_map))
                        .unwrap_or_default();
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str =
                        format!("for ({}; {}; {}) {{", init_s, test_expr, update_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    all_stmts_in_order.push(loop_str.clone());
                    memo_stmts.push(loop_str);
                }
                Terminal::ForOf {
                    init, loop_block, ..
                } => {
                    let init_expr = self.get_block_expr(*init, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        self.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else if !var_decl.is_empty() {
                            Vec::new()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} of {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    all_stmts_in_order.push(loop_str.clone());
                    memo_stmts.push(loop_str);
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    let init_expr = self.get_block_expr(*init, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        self.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else if !var_decl.is_empty() {
                            Vec::new()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} in {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    all_stmts_in_order.push(loop_str.clone());
                    memo_stmts.push(loop_str);
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = format!("while ({}) {{", test_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    all_stmts_in_order.push(loop_str.clone());
                    memo_stmts.push(loop_str);
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        self.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = "do {".to_string();
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str(&format!("\n}} while ({});", test_expr));
                    all_stmts_in_order.push(loop_str.clone());
                    memo_stmts.push(loop_str);
                }
                Terminal::If {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    let if_str = self.emit_if_terminal(
                        test,
                        *consequent,
                        *alternate,
                        *fallthrough,
                        &block_map,
                        &owned_blocks,
                    );
                    if let Some(s) = if_str {
                        all_stmts_in_order.push(s.clone());
                        memo_stmts.push(s);
                    }
                }
                Terminal::Try {
                    block: try_block,
                    handler_binding,
                    handler,
                    ..
                } => {
                    // If this is the top-level try wrapper, skip it here —
                    // the try body blocks are already processed inline in the main loop.
                    if try_wrapper.is_some() && !owned_blocks.contains(try_block) {
                        // Skip: try body is processed inline, wrapper applied at output
                    } else {
                        let try_stmts =
                            self.collect_loop_body_stmts(*try_block, &block_map, &owned_blocks);
                        // Empty try block elimination (pruneMaybeThrows equivalent):
                        // if the try body has no statements, the try-catch can't throw,
                        // so skip the entire try-catch
                        if try_stmts.is_empty() {
                            // Skip: empty try block
                        } else {
                            let catch_stmts =
                                self.collect_loop_body_stmts(*handler, &block_map, &owned_blocks);
                            let catch_param = handler_binding
                                .as_ref()
                                .map(|p| self.resolve_place(p))
                                .unwrap_or_default();
                            let mut try_str = "try {".to_string();
                            for s in &try_stmts {
                                try_str.push_str(&format!("\n  {}", s));
                            }
                            if catch_param.is_empty() {
                                try_str.push_str("\n} catch {");
                            } else {
                                try_str.push_str(&format!("\n}} catch ({}) {{", catch_param));
                            }
                            for s in &catch_stmts {
                                try_str.push_str(&format!("\n  {}", s));
                            }
                            try_str.push_str("\n}");
                            all_stmts_in_order.push(try_str.clone());
                            memo_stmts.push(try_str);
                        }
                    }
                }
                Terminal::Switch { test, cases, .. } => {
                    let test_expr = self.resolve_place(test);
                    let mut switch_str = format!("switch ({}) {{", test_expr);
                    for case in cases {
                        let case_stmts =
                            self.collect_loop_body_stmts(case.block, &block_map, &owned_blocks);
                        if let Some(test_place) = &case.test {
                            let case_test = self.resolve_place(test_place);
                            switch_str.push_str(&format!("\n  case {}:", case_test));
                        } else {
                            switch_str.push_str("\n  default:");
                        }
                        for s in &case_stmts {
                            switch_str.push_str(&format!("\n    {}", s));
                        }
                    }
                    switch_str.push_str("\n}");
                    all_stmts_in_order.push(switch_str.clone());
                    memo_stmts.push(switch_str);
                }
                _ => {}
            }

            match &block.terminal {
                Terminal::Return {
                    value,
                    return_variant,
                    ..
                } => {
                    if *return_variant == ReturnVariant::Explicit
                        || *return_variant == ReturnVariant::Implicit
                    {
                        return_expr = Some(self.resolve_place(value));
                    }
                }
                Terminal::Throw { value, .. } => {
                    let val = self.resolve_place(value);
                    let throw_stmt = format!("throw {};", val);
                    all_stmts_in_order.push(throw_stmt.clone());
                    memo_stmts.push(throw_stmt);
                }
                _ => {}
            }
        }

        // Reorder: move uninitialized `let <name>;` declarations before any assignments
        // to that variable. This fixes ordering when phi elimination + block iteration
        // produces reassignments before declarations.
        reorder_declarations(&mut memo_stmts);

        // Determine the memo variable name: use promoted var name if available, else "tN"
        // Determine the memo variable name: use promoted var name if available, else "tN"
        let memo_var = self
            .promoted_var
            .clone()
            .unwrap_or_else(|| format!("t{}", self.temp_start));
        // Determine the return variable: may differ from memo_var in scope output pattern
        let return_var = if self.is_scope_output {
            self.post_scope_return_var
                .clone()
                .unwrap_or_else(|| memo_var.clone())
        } else {
            memo_var.clone()
        };

        // Only add memoization if there are meaningful statements to memoize.
        let has_memo_content =
            !memo_stmts.is_empty() || return_expr.as_ref().is_some_and(|r| r != "undefined");
        let allocations_escape = self.allocations_escape(func);
        let is_trivial = self.is_trivial_body(func, &memo_stmts, &return_expr);
        let returns_primitive = self.returns_primitive_value(func);
        let needs_memo =
            has_memo_content && !is_trivial && allocations_escape && !returns_primitive;

        // Post-classification: hoist side-effect-only global calls out of memo block.
        // Only done when memoization will happen — otherwise statements stay in order.
        if needs_memo && !side_effect_hoistable.is_empty() {
            let mut hoisted: Vec<String> = Vec::new();
            memo_stmts.retain(|s| {
                if side_effect_hoistable.contains(s) {
                    hoisted.push(s.clone());
                    false
                } else {
                    true
                }
            });
            // Side-effect calls with prescope args go before the memo block
            pre_memo_stmts.extend(hoisted);
        }

        if needs_memo && return_expr.is_some() && !self.skip_memo {
            // Build transitive pre-scope set: expand pre_scope_instr_ids to include
            // all instructions that ONLY produce values consumed by pre-scope instructions.
            // This ensures that intermediate instructions (PropertyLoad, LoadLocal) feeding
            // into hook calls are also classified as pre-scope.
            // Only build filter when there are actually pre-scope instructions.
            let memo_filter: HashSet<InstructionId> = if pre_scope_instr_ids.is_empty() {
                HashSet::new()
            } else {
                // Build map: IdentifierId → InstructionId (which instruction produces it)
                let mut id_to_producer: HashMap<IdentifierId, InstructionId> = HashMap::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        id_to_producer.insert(instr.lvalue.identifier.id, instr.id);
                    }
                }

                // Build map: IdentifierId → set of InstructionIds that consume it
                let mut id_to_consumers: HashMap<IdentifierId, Vec<InstructionId>> = HashMap::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                            id_to_consumers
                                .entry(place.identifier.id)
                                .or_default()
                                .push(instr.id);
                        });
                    }
                }

                // Expand pre-scope: if ALL consumers of an instruction's output are pre-scope,
                // then the instruction itself is pre-scope too.
                let mut expanded_pre = pre_scope_instr_ids.clone();
                let mut changed = true;
                while changed {
                    changed = false;
                    for (_, block) in &func.body.blocks {
                        for instr in &block.instructions {
                            if expanded_pre.contains(&instr.id) {
                                continue; // Already marked
                            }
                            let out_id = instr.lvalue.identifier.id;
                            if let Some(consumers) = id_to_consumers.get(&out_id)
                                && !consumers.is_empty()
                                && consumers.iter().all(|c| expanded_pre.contains(c))
                            {
                                expanded_pre.insert(instr.id);
                                changed = true;
                            }
                        }
                    }
                }

                // Memo filter = all instruction IDs minus expanded pre-scope
                let mut all_ids: HashSet<InstructionId> = HashSet::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        all_ids.insert(instr.id);
                    }
                }
                all_ids.retain(|id| !expanded_pre.contains(id));
                all_ids
            };
            // Compute deps normally (heuristic + scope deps fallback).
            let heuristic_deps = self.find_reactive_deps(func);
            let mut reactive_deps = if heuristic_deps.is_empty() {
                self.find_scope_deps(func).unwrap_or(heuristic_deps)
            } else {
                heuristic_deps
            };
            // Post-filter: remove deps that are ONLY used in pre-scope instructions.
            // A dep is pre-scope-only if the filtered version doesn't include it.
            if !memo_filter.is_empty() && !reactive_deps.is_empty() {
                let filtered_deps = self.find_reactive_deps_filtered(func, Some(&memo_filter));
                reactive_deps.retain(|dep| filtered_deps.contains(dep));
            }

            // Substitute deps with local variable aliases when available.
            // If a dep like "something.StaticText1" is assigned to a variable
            // "let Foo = something.StaticText1" in the pre-memo statements,
            // use "Foo" as the dep key (matches upstream behavior).
            let dep_aliases = self.find_dep_aliases(func);
            for dep in &mut reactive_deps {
                if let Some(alias) = dep_aliases.get(dep.as_str()) {
                    *dep = alias.clone();
                }
            }

            // Collect extra scope outputs (reassignment + declaration outputs).
            // Uses direct HIR analysis rather than scope.declarations/reassignments
            // to handle fragmented scopes correctly.
            let (extra_outputs, _scope_count) = self.collect_extra_scope_outputs(func, &memo_var);
            let n_deps = reactive_deps.len();
            let n_results = 1 + extra_outputs.len(); // memo_var + extras

            let cv = &self.cache_var.clone();
            if reactive_deps.is_empty() {
                // No reactive deps: sentinel-based _c(N) pattern
                self.cache_slots = n_results as u32;
                self.emit_line(&format!("const {} = _c({});", cv, n_results));
            } else {
                // Dependency-based _c(N) pattern: N = deps + results
                self.cache_slots = (n_deps + n_results) as u32;
                self.emit_line(&format!("const {} = _c({});", cv, self.cache_slots));
            }

            // Emit pre-memo statements (hook calls)
            for stmt in &pre_memo_stmts {
                self.emit_line(stmt);
            }

            // Open try-catch wrapper if needed (try { goes before memo scope body)
            if try_wrapper.is_some() {
                self.emit_line("try {");
                self.indent += 1;
            }

            self.emit_line(&format!("let {};", memo_var));
            self.emitted_let_decls.insert(memo_var.clone());

            // Remove duplicate `let <memo_var>;` from memo_stmts — the declaration
            // was already emitted above, so any DeclareLocal in the collected stmts
            // would be a duplicate.
            let dup_decl = format!("let {};", memo_var);
            memo_stmts.retain(|s| s != &dup_decl);

            // Also remove duplicate `let` declarations for extra outputs
            for extra_name in &extra_outputs {
                let dup = format!("let {};", extra_name);
                memo_stmts.retain(|s| s != &dup);
            }

            if reactive_deps.is_empty() {
                self.emit_line(&format!(
                    "if ({}[0] === Symbol.for(\"react.memo_cache_sentinel\")) {{",
                    cv
                ));
            } else {
                let dep_checks: Vec<String> = reactive_deps
                    .iter()
                    .enumerate()
                    .map(|(i, dep)| format!("{}[{}] !== {}", cv, i, dep))
                    .collect();
                self.emit_line(&format!("if ({}) {{", dep_checks.join(" || ")));
            }
            self.indent += 1;

            for stmt in &memo_stmts {
                self.emit_line(stmt);
            }

            // Store dependencies and result
            let memo_already_assigned = demoted_return_var.is_some() || self.promoted_var.is_some();
            if reactive_deps.is_empty() {
                // Sentinel pattern: store memo_var in $[0], extras in $[1..]
                if memo_already_assigned {
                    self.emit_line(&format!("{}[0] = {};", cv, memo_var));
                } else if let Some(ref ret) = return_expr {
                    self.emit_line(&format!("{} = {};", memo_var, ret));
                    self.emit_line(&format!("{}[0] = {};", cv, memo_var));
                }
                // Store extra outputs
                for (i, name) in extra_outputs.iter().enumerate() {
                    self.emit_line(&format!("{}[{}] = {};", cv, 1 + i, name));
                }
            } else {
                // Dependency pattern: store deps in $[0..n-1], memo_var in $[n], extras in $[n+1..]
                if !memo_already_assigned && let Some(ref ret) = return_expr {
                    self.emit_line(&format!("{} = {};", memo_var, ret));
                }
                for (i, dep) in reactive_deps.iter().enumerate() {
                    self.emit_line(&format!("{}[{}] = {};", cv, i, dep));
                }
                let result_slot = n_deps;
                self.emit_line(&format!("{}[{}] = {};", cv, result_slot, memo_var));
                // Store extra outputs
                for (i, name) in extra_outputs.iter().enumerate() {
                    self.emit_line(&format!("{}[{}] = {};", cv, n_deps + 1 + i, name));
                }
            }

            self.indent -= 1;
            self.emit_line("} else {");
            self.indent += 1;
            let read_slot = if reactive_deps.is_empty() { 0 } else { n_deps };
            self.emit_line(&format!("{} = {}[{}];", memo_var, cv, read_slot));
            // Load extra outputs
            for (i, name) in extra_outputs.iter().enumerate() {
                let slot = if reactive_deps.is_empty() {
                    1 + i
                } else {
                    n_deps + 1 + i
                };
                self.emit_line(&format!("{} = {}[{}];", name, cv, slot));
            }
            self.indent -= 1;
            self.emit_line("}");

            // Emit post-scope statements (reads from cached variable)
            for stmt in &post_scope_stmts {
                self.emit_line(stmt);
            }

            // If we demoted the return var's StoreLocal, emit the named var assignment here
            if let Some(ref dvar) = demoted_return_var {
                self.emit_line(&format!("const {} = {};", dvar, memo_var));
            }

            self.emit_line(&format!("return {};", return_var));

            // Close try-catch wrapper if needed
            if let Some((ref handler_binding, handler_block)) = try_wrapper {
                let catch_stmts =
                    self.collect_loop_body_stmts(handler_block, &block_map, &owned_blocks);
                let catch_param = handler_binding
                    .as_ref()
                    .map(|p| self.resolve_place(p))
                    .unwrap_or_default();
                self.indent -= 1;
                if catch_param.is_empty() {
                    self.emit_line("} catch {");
                } else {
                    self.emit_line(&format!("}} catch ({}) {{", catch_param));
                }
                self.indent += 1;
                for s in &catch_stmts {
                    self.emit_line(s);
                }
                self.indent -= 1;
                self.emit_line("}");
            }
        } else {
            // When not memoizing, undo any memo-related transformations:
            // 1. promoted_store_ids: variables promoted for memo scope need declarations restored
            // 2. demoted_return_var: variables demoted to temp need to be restored
            // In either case, re-collect statements from scratch.
            let needs_rebuild = !self.promoted_store_ids.is_empty() || demoted_return_var.is_some();
            if needs_rebuild {
                self.promoted_store_ids.clear();
                // Re-collect the statements without promotion or demotion
                // Don't separate hooks — preserve source order since we're not memoizing
                let mut rebuilt_memo: Vec<String> = Vec::new();
                let mut rebuilt_return: Option<String> = None;
                for (_, block) in &func.body.blocks {
                    if owned_blocks.contains(&block.id) {
                        continue;
                    }
                    for instr in &block.instructions {
                        // No demotion — emit as-is, all in source order
                        if let Some(stmt) = self.instruction_to_stmt(instr) {
                            rebuilt_memo.push(stmt);
                        }
                    }
                    // Re-collect loop stmts from terminals
                    match &block.terminal {
                        Terminal::For {
                            init,
                            test,
                            update,
                            loop_block,
                            ..
                        } => {
                            let init_s = self.get_for_init_str(*init, &block_map);
                            let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                            let update_expr = update
                                .map(|u| self.get_for_update_expr(u, &block_map))
                                .unwrap_or_default();
                            let body_stmts = self.collect_loop_body_stmts(
                                *loop_block,
                                &block_map,
                                &owned_blocks,
                            );
                            let mut loop_str =
                                format!("for ({}; {}; {}) {{", init_s, test_expr, update_expr);
                            for s in &body_stmts {
                                loop_str.push_str(&format!("\n  {}", s));
                            }
                            loop_str.push_str("\n}");
                            rebuilt_memo.push(loop_str);
                        }
                        Terminal::While {
                            test, loop_block, ..
                        } => {
                            let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                            let body_stmts = self.collect_loop_body_stmts(
                                *loop_block,
                                &block_map,
                                &owned_blocks,
                            );
                            let mut loop_str = format!("while ({}) {{", test_expr);
                            for s in &body_stmts {
                                loop_str.push_str(&format!("\n  {}", s));
                            }
                            loop_str.push_str("\n}");
                            rebuilt_memo.push(loop_str);
                        }
                        Terminal::DoWhile {
                            loop_block, test, ..
                        } => {
                            let test_expr = self.get_block_expr_with_assignments(*test, &block_map);
                            let body_stmts = self.collect_loop_body_stmts(
                                *loop_block,
                                &block_map,
                                &owned_blocks,
                            );
                            let mut loop_str = "do {".to_string();
                            for s in &body_stmts {
                                loop_str.push_str(&format!("\n  {}", s));
                            }
                            loop_str.push_str(&format!("\n}} while ({});", test_expr));
                            rebuilt_memo.push(loop_str);
                        }
                        Terminal::ForOf {
                            init, loop_block, ..
                        } => {
                            let init_expr = self.get_block_expr(*init, &block_map);
                            let body_stmts = self.collect_loop_body_stmts(
                                *loop_block,
                                &block_map,
                                &owned_blocks,
                            );
                            let (var_decl, skip_count) =
                                self.get_for_in_of_var_decl(*loop_block, &block_map);
                            let rest_body: Vec<String> =
                                if !var_decl.is_empty() && body_stmts.len() > skip_count {
                                    body_stmts[skip_count..].to_vec()
                                } else if !var_decl.is_empty() {
                                    Vec::new()
                                } else {
                                    body_stmts.clone()
                                };
                            let mut loop_str = format!("for ({} of {}) {{", var_decl, init_expr);
                            for s in &rest_body {
                                loop_str.push_str(&format!("\n  {}", s));
                            }
                            loop_str.push_str("\n}");
                            rebuilt_memo.push(loop_str);
                        }
                        Terminal::ForIn {
                            init, loop_block, ..
                        } => {
                            let init_expr = self.get_block_expr(*init, &block_map);
                            let body_stmts = self.collect_loop_body_stmts(
                                *loop_block,
                                &block_map,
                                &owned_blocks,
                            );
                            let (var_decl, skip_count) =
                                self.get_for_in_of_var_decl(*loop_block, &block_map);
                            let rest_body: Vec<String> =
                                if !var_decl.is_empty() && body_stmts.len() > skip_count {
                                    body_stmts[skip_count..].to_vec()
                                } else if !var_decl.is_empty() {
                                    Vec::new()
                                } else {
                                    body_stmts.clone()
                                };
                            let mut loop_str = format!("for ({} in {}) {{", var_decl, init_expr);
                            for s in &rest_body {
                                loop_str.push_str(&format!("\n  {}", s));
                            }
                            loop_str.push_str("\n}");
                            rebuilt_memo.push(loop_str);
                        }
                        Terminal::If {
                            test,
                            consequent,
                            alternate,
                            fallthrough,
                            ..
                        }
                        | Terminal::Branch {
                            test,
                            consequent,
                            alternate,
                            fallthrough,
                            ..
                        } => {
                            let if_str = self.emit_if_terminal(
                                test,
                                *consequent,
                                *alternate,
                                *fallthrough,
                                &block_map,
                                &owned_blocks,
                            );
                            if let Some(s) = if_str {
                                rebuilt_memo.push(s);
                            }
                        }
                        Terminal::Try {
                            block: try_block,
                            handler_binding,
                            handler,
                            ..
                        } => {
                            if try_wrapper.is_some() && !owned_blocks.contains(try_block) {
                                // Top-level try wrapper — skip, handled at output
                            } else {
                                let try_stmts = self.collect_loop_body_stmts(
                                    *try_block,
                                    &block_map,
                                    &owned_blocks,
                                );
                                // Empty try block elimination
                                if try_stmts.is_empty() {
                                    // Skip: empty try block can't throw
                                } else {
                                    let catch_stmts = self.collect_loop_body_stmts(
                                        *handler,
                                        &block_map,
                                        &owned_blocks,
                                    );
                                    let catch_param = handler_binding
                                        .as_ref()
                                        .map(|p| self.resolve_place(p))
                                        .unwrap_or_default();
                                    let mut try_str = "try {".to_string();
                                    for s in &try_stmts {
                                        try_str.push_str(&format!("\n  {}", s));
                                    }
                                    if catch_param.is_empty() {
                                        try_str.push_str("\n} catch {");
                                    } else {
                                        try_str
                                            .push_str(&format!("\n}} catch ({}) {{", catch_param));
                                    }
                                    for s in &catch_stmts {
                                        try_str.push_str(&format!("\n  {}", s));
                                    }
                                    try_str.push_str("\n}");
                                    rebuilt_memo.push(try_str);
                                }
                            }
                        }
                        Terminal::Return {
                            value,
                            return_variant,
                            ..
                        } => {
                            if *return_variant == ReturnVariant::Explicit
                                || *return_variant == ReturnVariant::Implicit
                            {
                                let ret = self.resolve_place(value);
                                rebuilt_return = Some(ret);
                            }
                        }
                        _ => {}
                    }
                }
                reorder_declarations(&mut rebuilt_memo);

                // Eliminate `let <var>; return <var>;` for uninitialized variables
                let mut effective_rebuilt_return =
                    rebuilt_return.clone().or_else(|| return_expr.clone());
                if let Some(ref ret) = effective_rebuilt_return {
                    let uninit_decl = format!("let {};", ret);
                    let is_valid_ident = ret
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                    let has_assignment = rebuilt_memo.iter().any(|s| {
                        s.starts_with(&format!("{} = ", ret)) || s.contains(&format!(" {} = ", ret))
                    });
                    if is_valid_ident && rebuilt_memo.contains(&uninit_decl) && !has_assignment {
                        rebuilt_memo.retain(|s| s != &uninit_decl);
                        effective_rebuilt_return = None;
                    }
                }

                // No separate pre-emission needed — all stmts in rebuilt_memo in source order
                // Apply try-catch wrapper if needed (non-memoized rebuild path)
                if try_wrapper.is_some() {
                    self.emit_line("try {");
                    self.indent += 1;
                }
                for stmt in &rebuilt_memo {
                    self.emit_line(stmt);
                }
                // Use rebuilt return if we have one
                if let Some(ref ret) = effective_rebuilt_return
                    && ret != "undefined"
                {
                    self.emit_line(&format!("return {};", ret));
                }
                // Close try-catch wrapper if needed
                if let Some((ref handler_binding, handler_block)) = try_wrapper {
                    let catch_stmts =
                        self.collect_loop_body_stmts(handler_block, &block_map, &owned_blocks);
                    let catch_param = handler_binding
                        .as_ref()
                        .map(|p| self.resolve_place(p))
                        .unwrap_or_default();
                    self.indent -= 1;
                    if catch_param.is_empty() {
                        self.emit_line("} catch {");
                    } else {
                        self.emit_line(&format!("}} catch ({}) {{", catch_param));
                    }
                    self.indent += 1;
                    for s in &catch_stmts {
                        self.emit_line(s);
                    }
                    self.indent -= 1;
                    self.emit_line("}");
                }
            } else {
                // Non-memo path: use all_stmts_in_order to preserve source order
                // (avoids reordering caused by hook-call separation)
                let mut effective_return = return_expr.clone();
                let mut filtered_stmts = all_stmts_in_order.clone();
                reorder_declarations(&mut filtered_stmts);
                if let Some(ref ret) = return_expr {
                    let uninit_decl = format!("let {};", ret);
                    let is_valid_ident = ret
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
                    let has_assignment = filtered_stmts.iter().any(|s| {
                        s.starts_with(&format!("{} = ", ret)) || s.contains(&format!(" {} = ", ret))
                    });
                    if is_valid_ident && filtered_stmts.contains(&uninit_decl) && !has_assignment {
                        filtered_stmts.retain(|s| s != &uninit_decl);
                        effective_return = None;
                    }
                }

                // Apply try-catch wrapper if needed (simple non-memoized path)
                if let Some((ref _handler_binding, _handler_block)) = try_wrapper {
                    self.emit_line("try {");
                    self.indent += 1;
                }
                for stmt in &filtered_stmts {
                    self.emit_line(stmt);
                }
                if let Some(ref ret) = effective_return
                    && ret != "undefined"
                {
                    self.emit_line(&format!("return {};", ret));
                }
                // Close try-catch wrapper if needed
                if let Some((ref handler_binding, handler_block)) = try_wrapper {
                    let catch_stmts =
                        self.collect_loop_body_stmts(handler_block, &block_map, &owned_blocks);
                    let catch_param = handler_binding
                        .as_ref()
                        .map(|p| self.resolve_place(p))
                        .unwrap_or_default();
                    self.indent -= 1;
                    if catch_param.is_empty() {
                        self.emit_line("} catch {");
                    } else {
                        self.emit_line(&format!("}} catch ({}) {{", catch_param));
                    }
                    self.indent += 1;
                    for s in &catch_stmts {
                        self.emit_line(s);
                    }
                    self.indent -= 1;
                    self.emit_line("}");
                }
            }
        }
    }

    /// Collect statement strings from a block, following Goto chains.
    fn collect_block_stmts_str(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
        owned_blocks: &HashSet<BlockId>,
    ) -> Vec<String> {
        let mut stmts = Vec::new();
        let mut visited: HashSet<BlockId> = HashSet::new();
        self.collect_block_stmts_chain(block_id, block_map, owned_blocks, &mut stmts, &mut visited);
        stmts
    }

    /// Collect loop body statements, stripping the trailing `continue;` which is the
    /// natural loop continuation (not an explicit continue statement from user code).
    fn collect_loop_body_stmts(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
        owned_blocks: &HashSet<BlockId>,
    ) -> Vec<String> {
        let mut stmts = self.collect_block_stmts_str(block_id, block_map, owned_blocks);
        if stmts.last().is_some_and(|s| s == "continue;") {
            stmts.pop();
        }
        stmts
    }

    /// Inner recursive function that follows Goto chains.
    fn collect_block_stmts_chain(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
        owned_blocks: &HashSet<BlockId>,
        stmts: &mut Vec<String>,
        visited: &mut HashSet<BlockId>,
    ) {
        if !visited.insert(block_id) {
            return; // Avoid infinite loops
        }
        let Some(block) = block_map.get(&block_id) else {
            return;
        };
        for instr in &block.instructions {
            if let Some(stmt) = self.instruction_to_stmt(instr) {
                stmts.push(stmt);
            }
        }
        // Handle nested loop terminals within this block
        match &block.terminal {
            Terminal::For {
                init,
                test,
                update,
                loop_block,
                fallthrough,
                ..
            } => {
                let init_s = self.get_for_init_str(*init, block_map);
                let test_expr = self.get_block_expr_with_assignments(*test, block_map);
                let update_expr = update
                    .map(|u| self.get_for_update_expr(u, block_map))
                    .unwrap_or_default();
                let body_stmts = self.collect_loop_body_stmts(*loop_block, block_map, owned_blocks);
                let mut loop_str = format!("for ({}; {}; {}) {{", init_s, test_expr, update_expr);
                for s in &body_stmts {
                    loop_str.push_str(&format!("\n  {}", s));
                }
                loop_str.push_str("\n}");
                stmts.push(loop_str);
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::While {
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                let test_expr = self.get_block_expr_with_assignments(*test, block_map);
                let body_stmts = self.collect_loop_body_stmts(*loop_block, block_map, owned_blocks);
                let mut loop_str = format!("while ({}) {{", test_expr);
                for s in &body_stmts {
                    loop_str.push_str(&format!("\n  {}", s));
                }
                loop_str.push_str("\n}");
                stmts.push(loop_str);
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::DoWhile {
                loop_block,
                test,
                fallthrough,
                ..
            } => {
                let test_expr = self.get_block_expr_with_assignments(*test, block_map);
                let body_stmts = self.collect_loop_body_stmts(*loop_block, block_map, owned_blocks);
                let mut loop_str = "do {".to_string();
                for s in &body_stmts {
                    loop_str.push_str(&format!("\n  {}", s));
                }
                loop_str.push_str(&format!("\n}} while ({});", test_expr));
                stmts.push(loop_str);
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::ForOf {
                init,
                loop_block,
                fallthrough,
                ..
            } => {
                let init_expr = self.get_block_expr(*init, block_map);
                let body_stmts = self.collect_loop_body_stmts(*loop_block, block_map, owned_blocks);
                let (var_decl, skip_count) = self.get_for_in_of_var_decl(*loop_block, block_map);
                let rest_body: Vec<String> =
                    if !var_decl.is_empty() && body_stmts.len() > skip_count {
                        body_stmts[skip_count..].to_vec()
                    } else if !var_decl.is_empty() {
                        Vec::new()
                    } else {
                        body_stmts.clone()
                    };
                let mut loop_str = format!("for ({} of {}) {{", var_decl, init_expr);
                for s in &rest_body {
                    loop_str.push_str(&format!("\n  {}", s));
                }
                loop_str.push_str("\n}");
                stmts.push(loop_str);
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::ForIn {
                init,
                loop_block,
                fallthrough,
                ..
            } => {
                let init_expr = self.get_block_expr(*init, block_map);
                let body_stmts = self.collect_loop_body_stmts(*loop_block, block_map, owned_blocks);
                let (var_decl, skip_count) = self.get_for_in_of_var_decl(*loop_block, block_map);
                let rest_body: Vec<String> =
                    if !var_decl.is_empty() && body_stmts.len() > skip_count {
                        body_stmts[skip_count..].to_vec()
                    } else if !var_decl.is_empty() {
                        Vec::new()
                    } else {
                        body_stmts.clone()
                    };
                let mut loop_str = format!("for ({} in {}) {{", var_decl, init_expr);
                for s in &rest_body {
                    loop_str.push_str(&format!("\n  {}", s));
                }
                loop_str.push_str("\n}");
                stmts.push(loop_str);
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::If {
                test,
                consequent,
                alternate,
                fallthrough,
                ..
            }
            | Terminal::Branch {
                test,
                consequent,
                alternate,
                fallthrough,
                ..
            } => {
                // Use the non-sharing emit_if_terminal — branches get their own visited sets
                let if_str = self.emit_if_terminal(
                    test,
                    *consequent,
                    *alternate,
                    *fallthrough,
                    block_map,
                    owned_blocks,
                );
                if let Some(s) = if_str {
                    stmts.push(s);
                }
                // Follow the fallthrough chain for statements after the if
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::Try {
                block: try_block,
                handler_binding,
                handler,
                fallthrough,
                ..
            } => {
                let try_stmts = self.collect_loop_body_stmts(*try_block, block_map, owned_blocks);
                // Empty try block elimination
                if !try_stmts.is_empty() {
                    let catch_stmts =
                        self.collect_loop_body_stmts(*handler, block_map, owned_blocks);
                    let catch_param = handler_binding
                        .as_ref()
                        .map(|p| self.resolve_place(p))
                        .unwrap_or_default();
                    let mut try_str = "try {".to_string();
                    for s in &try_stmts {
                        try_str.push_str(&format!("\n  {}", s));
                    }
                    if catch_param.is_empty() {
                        try_str.push_str("\n} catch {");
                    } else {
                        try_str.push_str(&format!("\n}} catch ({}) {{", catch_param));
                    }
                    for s in &catch_stmts {
                        try_str.push_str(&format!("\n  {}", s));
                    }
                    try_str.push_str("\n}");
                    stmts.push(try_str);
                }
                self.collect_block_stmts_chain(
                    *fallthrough,
                    block_map,
                    owned_blocks,
                    stmts,
                    visited,
                );
            }
            Terminal::Return { value, .. } => {
                let ret = self.resolve_place(value);
                if ret == "undefined" {
                    stmts.push("return;".to_string());
                } else {
                    stmts.push(format!("return {};", ret));
                }
            }
            Terminal::Throw { value, .. } => {
                let val = self.resolve_place(value);
                stmts.push(format!("throw {};", val));
            }
            Terminal::Goto {
                variant: GotoVariant::Break,
                block,
                ..
            } => {
                // Only emit `break;` if the target is a loop fallthrough block
                if self.loop_fallthrough_blocks.contains(block) {
                    stmts.push("break;".to_string());
                }
                // Don't follow Break chains — the parent scope handles the fallthrough
            }
            Terminal::Goto {
                variant: GotoVariant::Continue,
                block,
                ..
            } => {
                // Only emit `continue;` if the target is a loop test block
                if self.loop_test_blocks.contains(block) {
                    stmts.push("continue;".to_string());
                }
                // Don't follow continue targets — they go back to loop test
            }
            Terminal::Goto { block, .. } => {
                // Follow the chain for non-Break/non-Continue gotos (e.g., Try)
                self.collect_block_stmts_chain(*block, block_map, owned_blocks, stmts, visited);
            }
            _ => {}
        }
    }

    /// Emit an if-statement from a Terminal::If or Terminal::Branch.
    /// Returns None if both branches are empty (the if can be elided).
    fn emit_if_terminal(
        &self,
        test: &Place,
        consequent: BlockId,
        alternate: BlockId,
        fallthrough: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
        owned_blocks: &HashSet<BlockId>,
    ) -> Option<String> {
        let test_expr = self.resolve_place(test);
        let cons_stmts = self.collect_block_stmts_str(consequent, block_map, owned_blocks);
        // Don't collect alternate stmts if alternate IS the fallthrough (no else branch)
        let alt_stmts = if alternate == fallthrough {
            Vec::new()
        } else {
            self.collect_block_stmts_str(alternate, block_map, owned_blocks)
        };

        // If both branches are empty, emit just the test as a statement if it could have side effects
        if cons_stmts.is_empty() && alt_stmts.is_empty() {
            // Emit `if (test) { }` to preserve side-effectful conditions
            return Some(format!("if ({}) {{  }}", test_expr));
        }

        let mut result = format!("if ({}) {{", test_expr);
        for s in &cons_stmts {
            result.push_str(&format!("\n  {}", s));
        }
        if alt_stmts.is_empty() {
            result.push_str("\n}");
        } else {
            result.push_str("\n} else {");
            for s in &alt_stmts {
                result.push_str(&format!("\n  {}", s));
            }
            result.push_str("\n}");
        }
        Some(result)
    }

    /// Get the expression from a test/condition block.
    /// The last instruction's expression is the condition value.
    fn get_block_expr(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
    ) -> String {
        let Some(block) = block_map.get(&block_id) else {
            return String::new();
        };
        // The last unnamed instruction's expression is the condition value.
        // Walk backward to find the last unnamed temporary instruction.
        for instr in block.instructions.iter().rev() {
            // Skip named variables (statements) — only look for unnamed temps
            if instr.lvalue.identifier.name.is_some() {
                continue;
            }
            // Try to resolve through expr_map first (most reliable)
            if let Some(expr) = self.expr_map.get(&instr.lvalue.identifier.id) {
                return expr.clone();
            }
            // Fallback to instruction_to_expr
            if let Some(expr) = self.instruction_to_expr(instr) {
                return expr;
            }
        }
        // If no unnamed temp found, try the last instruction's expression
        for instr in block.instructions.iter().rev() {
            if let Some(expr) = self.instruction_to_expr(instr) {
                return expr;
            }
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                if lvalue.kind == InstructionKind::Reassign {
                    // Assignment expression: reconstruct as `(name = rhs)`
                    let name = self.identifier_name(&lvalue.place.identifier);
                    let rhs = self.resolve_place(value);
                    return format!("({} = {})", name, rhs);
                }
                return self.resolve_place(value);
            }
        }
        String::new()
    }

    /// Like `get_block_expr`, but detects assignment expressions (`x = rhs`) in the
    /// block and inlines them into the result. Used for while/do-while test blocks
    /// where `while ((value = queue.pop()) != null)` needs to preserve the assignment.
    fn get_block_expr_with_assignments(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
    ) -> String {
        let Some(block) = block_map.get(&block_id) else {
            return String::new();
        };

        // Find any StoreLocal with Reassign kind. HIR decomposes
        // `while ((value = queue.pop()) != null)` into:
        //   t0 = queue.pop()
        //   value = t0 [Reassign]
        //   t1 = t0 != null
        // get_block_expr resolves to `queue.pop() != null`, losing the assignment.
        // We find the RHS expression in the resolved result and wrap it.
        let mut assignment_replacements: Vec<(String, String)> = Vec::new();
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                && lvalue.kind == InstructionKind::Reassign
            {
                let name = self.identifier_name(&lvalue.place.identifier);
                let rhs = self.resolve_place(value);
                assignment_replacements.push((name, rhs));
            }
        }

        if assignment_replacements.is_empty() {
            return self.get_block_expr(block_id, block_map);
        }

        let mut expr = self.get_block_expr(block_id, block_map);

        // Search for the RHS expression in the resolved result and wrap it
        // as `(name = rhs)`. The RHS (e.g. `queue.pop()`) appears in the
        // resolved expression but the variable name (`value`) does not.
        for (name, rhs) in &assignment_replacements {
            if let Some(pos) = expr.find(rhs.as_str()) {
                let replacement = format!("({} = {})", name, rhs);
                let mut result = String::with_capacity(expr.len() + name.len() + 5);
                result.push_str(&expr[..pos]);
                result.push_str(&replacement);
                result.push_str(&expr[pos + rhs.len()..]);
                expr = result;
            }
        }

        expr
    }

    /// Get the for-loop update expression as a string.
    /// Handles assignment expressions like `i = i + 1` which would otherwise
    /// be returned as just the RHS (`i + 1`) by `get_block_expr`.
    fn get_for_update_expr(
        &self,
        block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
    ) -> String {
        let Some(block) = block_map.get(&block_id) else {
            return String::new();
        };
        // Check if the last instruction is a StoreLocal with Reassign kind
        // In that case, the update expression is `name = rhs`
        if let Some(last_instr) = block.instructions.last()
            && let InstructionValue::StoreLocal { lvalue, value, .. } = &last_instr.value
            && lvalue.kind == InstructionKind::Reassign
        {
            let name = self.identifier_name(&lvalue.place.identifier);
            let rhs = self.resolve_place(value);
            return format!("{} = {}", name, rhs);
        }
        // Fall back to get_block_expr for simple update expressions (like i++)
        self.get_block_expr(block_id, block_map)
    }

    /// Get the for-loop init clause as a string.
    /// Merges multiple variable declarations into a single comma-separated declaration
    /// e.g., `let i = 0, length = arr.length` instead of `let i = 0; const length = arr.length`.
    fn get_for_init_str(
        &self,
        init_block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
    ) -> String {
        let Some(block) = block_map.get(&init_block_id) else {
            return String::new();
        };
        // Collect all variable declarations from StoreLocal instructions
        let mut decls: Vec<(String, String, String)> = Vec::new(); // (raw_keyword, name, value)
        for instr in &block.instructions {
            if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                let name = self.identifier_name(&lvalue.place.identifier);
                let val = self.resolve_place(value);
                // In for-loop init, keep the original keyword (don't promote let->const)
                // because for(let i = 0, j = 1; ...) requires a single keyword
                let keyword = match lvalue.kind {
                    InstructionKind::Const | InstructionKind::HoistedConst => "const",
                    InstructionKind::Let | InstructionKind::HoistedLet | InstructionKind::Catch => {
                        "let"
                    }
                    InstructionKind::Reassign => "",
                    InstructionKind::Function | InstructionKind::HoistedFunction => "",
                };
                decls.push((keyword.to_string(), name, val));
            }
        }

        if decls.is_empty() {
            // No variable declarations — might be an expression init
            // Fall back to collecting stmts normally
            let stmts: Vec<String> = block
                .instructions
                .iter()
                .filter_map(|instr| self.instruction_to_stmt(instr))
                .collect();
            return stmts.join(" ").trim_end_matches(';').to_string();
        }

        // Use the first declaration's keyword for all (they came from a single let/const statement)
        let keyword = &decls[0].0;
        let parts: Vec<String> = decls
            .iter()
            .map(|(_, name, val)| {
                if val == "undefined" {
                    name.clone()
                } else {
                    format!("{} = {}", name, val)
                }
            })
            .collect();
        if keyword.is_empty() {
            parts.join(", ")
        } else {
            format!("{} {}", keyword, parts.join(", "))
        }
    }

    /// Extract the loop variable declaration for for-in/for-of loops.
    /// Returns (declaration_string, num_body_stmts_to_skip).
    /// Handles both simple `const x` and destructuring `const { a, b }` or `const [a, b]` patterns.
    fn get_for_in_of_var_decl(
        &self,
        loop_block_id: BlockId,
        block_map: &HashMap<BlockId, &BasicBlock>,
    ) -> (String, usize) {
        let Some(block) = block_map.get(&loop_block_id) else {
            return (String::new(), 0);
        };

        // Check for a Destructure instruction at the start of the loop body.
        // For-of destructuring like `for (const { v } of items)` is lowered as:
        //   Primitive(Undefined) -> temp  (placeholder for iterator value)
        //   Destructure { lvalue: { pattern: Object/Array, kind }, value: temp }
        // We detect this and reconstruct the destructuring pattern in the loop header.
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::Primitive { .. } => {}
                InstructionValue::Destructure { lvalue, .. } => {
                    let keyword = match lvalue.kind {
                        InstructionKind::Const | InstructionKind::HoistedConst => "const",
                        InstructionKind::Let
                        | InstructionKind::HoistedLet
                        | InstructionKind::Catch => "let",
                        _ => "const",
                    };
                    let pattern_str = self.format_destructure_pattern(&lvalue.pattern);
                    // Skip 1 statement (the Destructure statement; Primitive doesn't emit a stmt)
                    return (format!("{} {}", keyword, pattern_str), 1);
                }
                _ => break,
            }
        }

        // Fall back: detect PropertyLoad+StoreLocal pairs pattern.
        let mut prop_loads: HashMap<IdentifierId, String> = HashMap::new();
        let mut prop_load_objects: HashMap<IdentifierId, IdentifierId> = HashMap::new();
        let mut destructure_props: Vec<(String, String)> = Vec::new();
        let mut first_store_local_idx: Option<usize> = None;
        let mut keyword = "const";
        let mut stmts_to_skip = 0;
        let mut common_source: Option<IdentifierId> = None;
        let mut is_destructure = false;

        for (i, instr) in block.instructions.iter().enumerate() {
            match &instr.value {
                InstructionValue::PropertyLoad {
                    object, property, ..
                } => {
                    if let PropertyLiteral::String(key) = property {
                        prop_loads.insert(instr.lvalue.identifier.id, key.clone());
                        prop_load_objects.insert(instr.lvalue.identifier.id, object.identifier.id);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. } => {
                    let name = self.identifier_name(&lvalue.place.identifier);

                    if first_store_local_idx.is_none() {
                        first_store_local_idx = Some(i);
                        keyword = match lvalue.kind {
                            InstructionKind::Const | InstructionKind::HoistedConst => "const",
                            InstructionKind::Let
                            | InstructionKind::HoistedLet
                            | InstructionKind::Catch => "let",
                            _ => "const",
                        };
                    }

                    // Check if this StoreLocal's value comes from a PropertyLoad
                    if let Some(prop_key) = prop_loads.get(&value.identifier.id) {
                        let source_obj = prop_load_objects[&value.identifier.id];
                        if let Some(cs) = common_source {
                            if cs != source_obj {
                                break;
                            }
                        } else {
                            common_source = Some(source_obj);
                        }
                        is_destructure = true;
                        if prop_key == &name {
                            destructure_props.push((prop_key.clone(), String::new()));
                        } else {
                            destructure_props.push((prop_key.clone(), name.clone()));
                        }
                        stmts_to_skip += 1;
                    } else if destructure_props.is_empty() {
                        // Simple (non-destructured) for-of variable: `const x`
                        return (format!("{} {}", keyword, name), 1);
                    } else {
                        break;
                    }
                }
                _ => {
                    if first_store_local_idx.is_some() {
                        break;
                    }
                }
            }
        }

        if is_destructure && !destructure_props.is_empty() {
            let props_str = destructure_props
                .iter()
                .map(|(key, binding)| {
                    if binding.is_empty() {
                        key.clone()
                    } else {
                        format!("{}: {}", key, binding)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            return (format!("{} {{ {} }}", keyword, props_str), stmts_to_skip);
        }

        (String::new(), 0)
    }

    /// Format a destructuring pattern (for-in/for-of variable declarations).
    fn format_destructure_pattern(&self, pattern: &Pattern) -> String {
        match pattern {
            Pattern::Object(obj) => {
                let props: Vec<String> =
                    obj.properties
                        .iter()
                        .map(|p| match p {
                            ObjectPropertyOrSpread::Property(prop) => {
                                let key = match &prop.key {
                                    ObjectPropertyKey::String(s)
                                    | ObjectPropertyKey::Identifier(s) => s.clone(),
                                    ObjectPropertyKey::Number(n) => n.to_string(),
                                    ObjectPropertyKey::Computed(place) => {
                                        format!("[{}]", self.resolve_place(place))
                                    }
                                };
                                let name = self.identifier_name(&prop.place.identifier);
                                if key == name {
                                    key
                                } else {
                                    format!("{}: {}", key, name)
                                }
                            }
                            ObjectPropertyOrSpread::Spread(place) => {
                                format!("...{}", self.identifier_name(&place.identifier))
                            }
                        })
                        .collect();
                format!("{{ {} }}", props.join(", "))
            }
            Pattern::Array(arr) => {
                let items: Vec<String> = arr
                    .items
                    .iter()
                    .map(|item| match item {
                        ArrayElement::Place(p) => self.identifier_name(&p.identifier),
                        ArrayElement::Spread(p) => {
                            format!("...{}", self.identifier_name(&p.identifier))
                        }
                        ArrayElement::Hole => String::new(),
                    })
                    .collect();
                format!("[{}]", items.join(", "))
            }
        }
    }

    /// Try to use scope dependencies populated by propagate_scope_dependencies pass.
    /// Returns None if no scope has populated dependencies, so caller should fall back to heuristic.
    /// Merges deps from ALL scopes (for single-scope codegen treating the whole function as one scope).
    fn find_scope_deps(&self, func: &HIRFunction) -> Option<Vec<String>> {
        let mut deps: Vec<String> = Vec::new();
        let mut seen_scopes: HashSet<ScopeId> = HashSet::new();
        let mut has_any_scope = false;

        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let Some(scope) = &instr.lvalue.identifier.scope {
                    has_any_scope = true;
                    if !seen_scopes.insert(scope.id) {
                        continue; // Already processed this scope
                    }
                    for dep in &scope.dependencies {
                        let name = match &dep.identifier.name {
                            Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                                n.clone()
                            }
                            None => continue, // Skip unnamed identifiers
                        };
                        let dep_str = if dep.path.is_empty() {
                            name
                        } else {
                            let mut full_path = name;
                            for entry in &dep.path {
                                if entry.optional {
                                    full_path = format!("{}?.{}", full_path, entry.property);
                                } else {
                                    full_path = format!("{}.{}", full_path, entry.property);
                                }
                            }
                            full_path
                        };
                        if !deps.contains(&dep_str) {
                            deps.push(dep_str);
                        }
                    }
                }
            }
        }

        if !has_any_scope {
            // No scopes at all — return None so caller uses heuristic
            return None;
        }

        if deps.is_empty() {
            // Scopes exist but all deps were pruned (sentinel case)
            Some(Vec::new())
        } else {
            // Sort deps in param order to match find_reactive_deps output.
            // Collect param names in order.
            let param_order: Vec<String> = func
                .params
                .iter()
                .filter_map(|p| {
                    let place = match p {
                        Argument::Place(pp) => pp,
                        Argument::Spread(pp) => pp,
                    };
                    match &place.identifier.name {
                        Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                            Some(n.clone())
                        }
                        None => None,
                    }
                })
                .collect();
            deps.sort_by(|a, b| {
                let a_base = a.split('.').next().unwrap_or(a);
                let b_base = b.split('.').next().unwrap_or(b);
                let a_idx = param_order
                    .iter()
                    .position(|p| p == a_base)
                    .unwrap_or(usize::MAX);
                let b_idx = param_order
                    .iter()
                    .position(|p| p == b_base)
                    .unwrap_or(usize::MAX);
                a_idx.cmp(&b_idx).then_with(|| a.cmp(b))
            });
            Some(deps)
        }
    }

    /// Collect extra scope outputs (declarations + reassignments) that need to be cached
    /// beyond the memo_var.
    ///
    /// Instead of relying on scope.declarations/reassignments (which may be incomplete
    /// due to fragmented scopes), directly analyzes the HIR to detect:
    ///
    /// 1. **Reassignment outputs**: Variables whose DeclareLocal is BEFORE the earliest
    ///    scope boundary, AND that have Reassign-kind stores INSIDE any reactive scope.
    ///    These are pre-scope variables modified inside the scope.
    ///
    /// 2. **Declaration outputs**: Variables whose DeclareLocal is AT/AFTER the scope
    ///    boundary, AND that are used in the return terminal. These are scope-local
    ///    variables that escape the scope through the return value.
    ///
    /// Returns (extra_output_names, scope_count).
    fn collect_extra_scope_outputs(
        &self,
        func: &HIRFunction,
        memo_var: &str,
    ) -> (Vec<String>, usize) {
        let debug = std::env::var("DEBUG_SCOPES").is_ok();

        // Step 1: Collect all reactive scopes and find the scope boundary.
        let mut scope_ranges: Vec<(ScopeId, InstructionId, InstructionId)> = Vec::new();
        let mut seen_scopes: HashSet<ScopeId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let Some(ref scope) = instr.lvalue.identifier.scope
                    && seen_scopes.insert(scope.id)
                {
                    scope_ranges.push((scope.id, scope.range.start, scope.range.end));
                }
            }
        }
        let scope_count = scope_ranges.len();
        if scope_count == 0 {
            return (Vec::new(), 0);
        }

        // Scope boundary = earliest scope start
        let scope_boundary = scope_ranges.iter().map(|s| s.1).min().unwrap();
        // Latest scope end (for checking if instructions are inside any scope)
        let scope_end = scope_ranges.iter().map(|s| s.2).max().unwrap();

        if debug {
            eprintln!(
                "[SCOPE_OUTPUTS] {} scopes, boundary={}, end={}",
                scope_count, scope_boundary.0, scope_end.0
            );
        }

        // Helper: check if an instruction ID is inside any reactive scope
        let is_in_scope = |instr_id: InstructionId| -> bool {
            scope_ranges
                .iter()
                .any(|(_, start, end)| instr_id >= *start && instr_id < *end)
        };

        // Step 2: Collect DeclareLocal positions and names by declaration_id.
        let mut decl_instr: HashMap<DeclarationId, InstructionId> = HashMap::new();
        let mut decl_names: HashMap<DeclarationId, String> = HashMap::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::DeclareLocal { lvalue, .. } = &instr.value {
                    decl_instr
                        .entry(lvalue.place.identifier.declaration_id)
                        .or_insert(instr.id);
                    if let Some(IdentifierName::Named(name)) = &lvalue.place.identifier.name {
                        decl_names
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_insert_with(|| name.clone());
                    }
                }
            }
        }

        // Step 3: Find reassignment outputs — variables declared BEFORE the scope
        // boundary that have Reassign-kind stores INSIDE any reactive scope.
        let mut reassignment_outputs: Vec<(DeclarationId, String)> = Vec::new();
        let mut output_decl_ids: HashSet<DeclarationId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && lvalue.kind == InstructionKind::Reassign
                    && is_in_scope(instr.id)
                {
                    let did = lvalue.place.identifier.declaration_id;
                    // Check: DeclareLocal must be before the scope boundary
                    if let Some(&decl_id) = decl_instr.get(&did)
                        && decl_id < scope_boundary
                        && !output_decl_ids.contains(&did)
                        && let Some(name) = decl_names.get(&did)
                        && !Self::is_temp_name(name)
                        && name != memo_var
                    {
                        if debug {
                            eprintln!(
                                "[SCOPE_OUTPUTS] reassignment: {} (decl@{}, store@{} in scope)",
                                name, decl_id.0, instr.id.0
                            );
                        }
                        output_decl_ids.insert(did);
                        reassignment_outputs.push((did, name.clone()));
                    }
                }
            }
        }

        // Step 4: Find declaration outputs — variables used in the return terminal
        // whose DeclareLocal is AT or AFTER the scope boundary (scope-local variables
        // that escape through return).
        // Trace the return value through LoadLocals to find the named variable.
        let mut return_decl_ids: HashSet<DeclarationId> = HashSet::new();
        let mut load_source: HashMap<IdentifierId, (IdentifierId, DeclarationId)> = HashMap::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::LoadLocal { place, .. } = &instr.value {
                    load_source.insert(
                        instr.lvalue.identifier.id,
                        (place.identifier.id, place.identifier.declaration_id),
                    );
                }
            }
            if let Terminal::Return { value, .. } = &block.terminal {
                // Trace backward through LoadLocals
                let mut cur_id = value.identifier.id;
                for _ in 0..20 {
                    if let Some(&(src_id, decl_id)) = load_source.get(&cur_id) {
                        return_decl_ids.insert(decl_id);
                        cur_id = src_id;
                    } else {
                        break;
                    }
                }
                // Also add the direct operand's declaration_id
                return_decl_ids.insert(value.identifier.declaration_id);
            }
        }

        let mut declaration_outputs: Vec<(DeclarationId, String)> = Vec::new();
        for decl_id in &return_decl_ids {
            if output_decl_ids.contains(decl_id) {
                continue; // Already a reassignment output
            }
            if let Some(&decl_instr_id) = decl_instr.get(decl_id) {
                // Only add if DeclareLocal is at or after the scope boundary
                // (scope-local variables that escape through return)
                if decl_instr_id >= scope_boundary
                    && let Some(name) = decl_names.get(decl_id)
                    && !Self::is_temp_name(name)
                    && name != memo_var
                {
                    if debug {
                        eprintln!(
                            "[SCOPE_OUTPUTS] declaration (return-used): {} (decl@{}, boundary={})",
                            name, decl_instr_id.0, scope_boundary.0
                        );
                    }
                    output_decl_ids.insert(*decl_id);
                    declaration_outputs.push((*decl_id, name.clone()));
                }
            }
        }

        // Combine outputs: reassignments first, then declarations (matches upstream ordering)
        let mut outputs: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        seen.insert(memo_var.to_string());
        for (_, name) in &reassignment_outputs {
            if seen.insert(name.clone()) {
                outputs.push(name.clone());
            }
        }
        for (_, name) in &declaration_outputs {
            if seen.insert(name.clone()) {
                outputs.push(name.clone());
            }
        }

        if debug && !outputs.is_empty() {
            eprintln!("[SCOPE_OUTPUTS] final outputs: {:?}", outputs);
        }

        (outputs, scope_count)
    }

    /// Check if a name looks like a compiler temporary (t0, t1, _t0, _t1, etc.)
    fn is_temp_name(name: &str) -> bool {
        if name.starts_with('t') && name.len() > 1 && name[1..].chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
        if name.starts_with("_t") && name.len() > 2 && name[2..].chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
        false
    }

    /// Find reactive dependencies: function parameters (or their property accesses) used in the body.
    /// These determine whether the cached result should be invalidated.
    ///
    /// If `props` is only accessed via property loads like `props.a`, `props.b`, the dependencies
    /// are the individual properties. If `props` is used directly (e.g., passed to a function),
    /// then `props` itself is the dependency.
    ///
    /// When `memo_filter` is Some, only count "leaf uses" from instructions in the filter set.
    /// PropertyLoad chains are still traced regardless of filter (they don't consume the value),
    /// but the final non-PropertyLoad uses only count if the consuming instruction is in the filter.
    fn find_reactive_deps_filtered(
        &self,
        func: &HIRFunction,
        memo_filter: Option<&HashSet<InstructionId>>,
    ) -> Vec<String> {
        // Collect parameter names and IDs
        let mut param_names: Vec<String> = Vec::new();
        let mut anon_params: Vec<(IdentifierId, DeclarationId)> = Vec::new();
        for param in &func.params {
            match param {
                Argument::Place(place) => {
                    if let Some(IdentifierName::Named(name))
                    | Some(IdentifierName::Promoted(name)) = &place.identifier.name
                    {
                        param_names.push(name.clone());
                    } else {
                        anon_params.push((place.identifier.id, place.identifier.declaration_id));
                    }
                }
                Argument::Spread(place) => {
                    if let Some(IdentifierName::Named(name))
                    | Some(IdentifierName::Promoted(name)) = &place.identifier.name
                    {
                        param_names.push(name.clone());
                    } else {
                        anon_params.push((place.identifier.id, place.identifier.declaration_id));
                    }
                }
            }
        }

        // Collect hook call result names as reactive values.
        // Not all hook results are reactive:
        // - useRef: returns a stable ref object (NOT reactive)
        // - useState: [state, setter] — state is reactive, setter is stable
        // - useReducer: [state, dispatch] — state is reactive, dispatch is stable
        // - useActionState: [state, dispatch, pending] — state+pending reactive, dispatch stable
        // - Other hooks: assume result is reactive (conservative)
        let mut hook_result_names: Vec<String> = Vec::new();
        let mut hook_result_ids: HashSet<IdentifierId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        if let Some(expr) = self.expr_map.get(&value.identifier.id)
                            && self.looks_like_hook_call(expr)
                        {
                            let hook_name_str = expr.split('(').next().unwrap_or("");
                            // Skip hooks whose direct return value is stable
                            if Self::is_stable_hook_return(hook_name_str) {
                                continue;
                            }
                            let name = self.identifier_name(&lvalue.place.identifier);
                            if !name.starts_with('_') {
                                hook_result_names.push(name);
                                hook_result_ids.insert(lvalue.place.identifier.id);
                            }
                        }
                    }
                    InstructionValue::Destructure { lvalue, value, .. } => {
                        if let Some(expr) = self.expr_map.get(&value.identifier.id)
                            && self.looks_like_hook_call(expr)
                        {
                            let hook_name_str = expr.split('(').next().unwrap_or("");
                            // For destructured results, check each position
                            match &lvalue.pattern {
                                Pattern::Array(arr) => {
                                    for (idx, elem) in arr.items.iter().enumerate() {
                                        if let ArrayElement::Place(p) = elem
                                            && let Some(IdentifierName::Named(n))
                                            | Some(IdentifierName::Promoted(n)) =
                                                &p.identifier.name
                                            && Self::is_reactive_destructured_element(
                                                hook_name_str,
                                                idx,
                                            )
                                        {
                                            hook_result_names.push(n.clone());
                                            hook_result_ids.insert(p.identifier.id);
                                        }
                                    }
                                }
                                Pattern::Object(obj) => {
                                    for prop in &obj.properties {
                                        if let ObjectPropertyOrSpread::Property(p) = prop
                                            && let Some(IdentifierName::Named(n))
                                            | Some(IdentifierName::Promoted(n)) =
                                                &p.place.identifier.name
                                        {
                                            // Object destructuring of hook results: be conservative, treat as reactive
                                            if !Self::is_stable_hook_return(hook_name_str) {
                                                hook_result_names.push(n.clone());
                                                hook_result_ids.insert(p.place.identifier.id);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // For anonymous params (destructured), find what variables are derived from them
        // by tracing through Destructure and StoreLocal instructions.
        if param_names.is_empty() && !anon_params.is_empty() {
            let mut deps = self.find_deps_from_anon_params(func, &anon_params);
            // Also add hook result deps
            for hr_name in &hook_result_names {
                if !deps.contains(hr_name) {
                    deps.push(hr_name.clone());
                }
            }
            return deps;
        }

        if param_names.is_empty() && hook_result_names.is_empty() {
            return Vec::new();
        }

        // For each parameter, find how it's used:
        // - If only accessed via PropertyLoad (e.g., props.x, props.y), collect the property paths
        // - If directly used (passed to function, in expression), use the parameter itself
        let mut all_deps: Vec<String> = Vec::new();

        for param_name in &param_names {
            let mut direct_use = false;
            let mut param_load_ids: HashSet<IdentifierId> = HashSet::new();

            // First pass: find all LoadLocal/LoadGlobal instructions for this parameter.
            // LoadGlobal matches param names when IIFE inlining moves inner function
            // instructions into the parent scope — the inner fn referenced the param
            // as a captured free variable (LoadGlobal), not a local.
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. } => {
                            if let Some(IdentifierName::Named(n))
                            | Some(IdentifierName::Promoted(n)) = &place.identifier.name
                                && n == param_name
                            {
                                param_load_ids.insert(instr.lvalue.identifier.id);
                            }
                        }
                        InstructionValue::LoadGlobal { binding, .. } => {
                            let binding_name = match binding {
                                NonLocalBinding::Global { name } => Some(name),
                                NonLocalBinding::ModuleLocal { name } => Some(name),
                                _ => None,
                            };
                            if let Some(name) = binding_name
                                && name == param_name
                            {
                                param_load_ids.insert(instr.lvalue.identifier.id);
                            }
                        }
                        _ => {}
                    }
                }
            }

            if param_load_ids.is_empty() {
                continue;
            }

            // Second pass: check how each LoadLocal result is consumed.
            // Track through PropertyLoad chains: props → props.a → props.a.b → etc.
            // Maps IdentifierId → the property path string (e.g., "props.a.b")
            let mut id_to_path: HashMap<IdentifierId, String> = HashMap::new();
            for &pid in &param_load_ids {
                id_to_path.insert(pid, param_name.clone());
            }

            // Multi-pass: resolve PropertyLoad chains to their full paths
            let mut changed = true;
            while changed {
                changed = false;
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        if let InstructionValue::PropertyLoad {
                            object,
                            property,
                            optional,
                            ..
                        } = &instr.value
                        {
                            if id_to_path.contains_key(&instr.lvalue.identifier.id) {
                                continue; // Already resolved
                            }
                            if let Some(base_path) = id_to_path.get(&object.identifier.id) {
                                let prop_str = match property {
                                    PropertyLiteral::String(s) => s.clone(),
                                    PropertyLiteral::Number(n) => n.to_string(),
                                };
                                let sep = if *optional { "?." } else { "." };
                                let full_path = format!("{}{}{}", base_path, sep, prop_str);
                                id_to_path.insert(instr.lvalue.identifier.id, full_path);
                                changed = true;
                            }
                        }
                    }
                }
            }

            // Now find the "leaf" uses — identifiers from the chain that are consumed
            // by non-PropertyLoad instructions. These are the actual dependencies.
            // When memo_filter is Some, only count uses from instructions in the filter.
            let mut leaf_paths: Vec<String> = Vec::new();
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    // Skip non-memo instructions when filter is active
                    let in_memo = memo_filter.is_none_or(|f| f.contains(&instr.id));
                    match &instr.value {
                        InstructionValue::PropertyLoad { .. } => {
                            // PropertyLoad extends the chain — always traced regardless of filter
                        }
                        _ => {
                            if in_memo {
                                // Check if any operand references a tracked ID
                                crate::hir::visitors::for_each_instruction_operand(
                                    instr,
                                    |place| {
                                        if let Some(path) = id_to_path.get(&place.identifier.id) {
                                            // This is a non-PropertyLoad use of a tracked path
                                            if param_load_ids.contains(&place.identifier.id) {
                                                // Direct use of parameter (not through property chain)
                                                direct_use = true;
                                            } else if !leaf_paths.contains(path) {
                                                leaf_paths.push(path.clone());
                                            }
                                        }
                                    },
                                );
                            }
                        }
                    }
                }
                // Check terminal references only if block has memo instructions
                let block_in_memo = memo_filter
                    .is_none_or(|f| block.instructions.iter().any(|i| f.contains(&i.id)));
                if block_in_memo {
                    let mut check_terminal_place = |place: &Place| {
                        if let Some(path) = id_to_path.get(&place.identifier.id) {
                            if param_load_ids.contains(&place.identifier.id) {
                                direct_use = true;
                            } else if !leaf_paths.contains(path) {
                                leaf_paths.push(path.clone());
                            }
                        }
                    };
                    match &block.terminal {
                        Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                            check_terminal_place(value);
                        }
                        Terminal::If { test, .. }
                        | Terminal::Branch { test, .. }
                        | Terminal::Switch { test, .. } => {
                            check_terminal_place(test);
                        }
                        _ => {}
                    }
                }
            }

            // Deduplicate: if a shorter prefix is already in the list, remove sub-paths.
            // e.g., if both "props.items" and "props.items.length" are present, keep only "props.items"
            leaf_paths.sort();
            let mut deduped: Vec<String> = Vec::new();
            for path in &leaf_paths {
                let has_prefix = deduped.iter().any(|existing| {
                    path.starts_with(existing.as_str())
                        && path.as_bytes().get(existing.len()) == Some(&b'.')
                });
                if !has_prefix {
                    // Also remove any existing paths that are sub-paths of this one
                    deduped.retain(|existing| {
                        !(existing.starts_with(path.as_str())
                            && existing.as_bytes().get(path.len()) == Some(&b'.'))
                    });
                    deduped.push(path.clone());
                }
            }
            let mut property_accesses = deduped;

            if direct_use {
                // Parameter is used directly (not only via properties) — use the whole parameter
                all_deps.push(param_name.clone());
            } else if !property_accesses.is_empty() {
                // Parameter is only accessed via properties — use individual properties
                // Sort for deterministic output
                property_accesses.sort();
                all_deps.extend(property_accesses);
            } else if memo_filter.is_none() {
                // Safety fallback (no filter): param was loaded but we detected no specific usage.
                // Conservatively add the whole parameter.
                all_deps.push(param_name.clone());
            }
            // When memo_filter is active and no memo-scope uses found, skip this param entirely.
        }

        // Add hook result names as reactive dependencies.
        // Hook results like `const id = useSelectedEntitytId()` are reactive — they change
        // between renders and should be tracked as dependencies.
        for hook_name in &hook_result_names {
            if !all_deps.contains(hook_name) {
                all_deps.push(hook_name.clone());
            }
        }

        // Sort for deterministic output (matching upstream dep ordering)
        all_deps.sort();
        all_deps.dedup();
        all_deps
    }

    /// Convenience wrapper: find reactive deps without filtering.
    fn find_reactive_deps(&self, func: &HIRFunction) -> Vec<String> {
        self.find_reactive_deps_filtered(func, None)
    }

    /// Find dep-to-variable-alias mappings.
    /// When a dep expression like "something.StaticText1" is assigned to a named
    /// local variable via `let Foo = something.StaticText1`, upstream uses "Foo"
    /// as the dependency key instead of the expression. This method builds a mapping
    /// from expression strings to their variable aliases.
    fn find_dep_aliases(&self, func: &HIRFunction) -> HashMap<String, String> {
        // Collect parameter names to avoid aliasing param.prop expressions
        let mut param_names: HashSet<String> = HashSet::new();
        for param in &func.params {
            let place = match param {
                Argument::Place(p) | Argument::Spread(p) => p,
            };
            if let Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) =
                &place.identifier.name
            {
                param_names.insert(n.clone());
            }
        }

        let mut aliases: HashMap<String, String> = HashMap::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                    && let Some(expr) = self.expr_map.get(&value.identifier.id)
                {
                    // Only alias property access expressions NOT rooted at a parameter.
                    // "props.x" should stay as "props.x" (param property),
                    // but "something.StaticText1" can be aliased to "Foo".
                    if !expr.contains('(') && expr.contains('.') {
                        let base = expr.split('.').next().unwrap_or("");
                        let base_no_opt = base.trim_end_matches('?');
                        if !param_names.contains(base_no_opt) {
                            let var_name = self.identifier_name(&lvalue.place.identifier);
                            let is_temp = var_name.starts_with('_')
                                || (var_name.starts_with('t')
                                    && var_name.len() > 1
                                    && var_name[1..].chars().all(|c| c.is_ascii_digit()));
                            if !is_temp {
                                aliases.insert(expr.clone(), var_name);
                            }
                        }
                    }
                }
            }
        }
        aliases
    }

    /// Find reactive dependencies from anonymous (destructured) parameters.
    /// Traces through Destructure and StoreLocal instructions to find variables
    /// derived from the anonymous param, then tracks their usage as dependencies.
    fn find_deps_from_anon_params(
        &self,
        func: &HIRFunction,
        anon_params: &[(IdentifierId, DeclarationId)],
    ) -> Vec<String> {
        let mut all_deps: Vec<String> = Vec::new();

        for &(param_id, param_decl_id) in anon_params {
            // Find variables derived from this anonymous param via Destructure or StoreLocal.
            // After SSA, the param ID gets a new SSA ID. We need to find the SSA'd version.
            // The Destructure instruction's value operand should reference the param.
            let mut derived_var_names: Vec<String> = Vec::new();
            let mut derived_var_ids: HashSet<IdentifierId> = HashSet::new();

            // Find LoadLocal instructions that load from identifiers with the same declaration_id
            // as the param, or that load from the param's SSA ID.
            let mut param_load_ids: HashSet<IdentifierId> = HashSet::new();
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    if let InstructionValue::LoadLocal { place, .. } = &instr.value
                        && (place.identifier.declaration_id == param_decl_id
                            || place.identifier.id == param_id)
                    {
                        param_load_ids.insert(instr.lvalue.identifier.id);
                    }
                }
            }

            // Find Destructure instructions that consume the param load
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    if let InstructionValue::Destructure { lvalue, value, .. } = &instr.value
                        && (param_load_ids.contains(&value.identifier.id)
                            || value.identifier.id == param_id)
                    {
                        // Extract variable names from the destructuring pattern
                        match &lvalue.pattern {
                            Pattern::Object(obj) => {
                                for prop in &obj.properties {
                                    if let ObjectPropertyOrSpread::Property(p) = prop
                                        && let Some(IdentifierName::Named(name))
                                        | Some(IdentifierName::Promoted(name)) =
                                            &p.place.identifier.name
                                    {
                                        derived_var_names.push(name.clone());
                                        derived_var_ids.insert(p.place.identifier.id);
                                    }
                                }
                            }
                            Pattern::Array(arr) => {
                                for elem in &arr.items {
                                    if let ArrayElement::Place(p) = elem
                                        && let Some(IdentifierName::Named(name))
                                        | Some(IdentifierName::Promoted(name)) =
                                            &p.identifier.name
                                    {
                                        derived_var_names.push(name.clone());
                                        derived_var_ids.insert(p.identifier.id);
                                    }
                                }
                            }
                        }
                    }
                    // Also check StoreLocal that consume the param load (for simple destructuring
                    // like `const x = params[0]` after lowering)
                    if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value
                        && (param_load_ids.contains(&value.identifier.id)
                            || value.identifier.id == param_id)
                        && let Some(IdentifierName::Named(name))
                        | Some(IdentifierName::Promoted(name)) = &lvalue.place.identifier.name
                    {
                        derived_var_names.push(name.clone());
                        derived_var_ids.insert(lvalue.place.identifier.id);
                    }
                    // Check PropertyLoad from param (for properties accessed on destructured vars)
                    if let InstructionValue::PropertyLoad {
                        object, property, ..
                    } = &instr.value
                        && (param_load_ids.contains(&object.identifier.id)
                            || object.identifier.id == param_id)
                        && let PropertyLiteral::String(prop) = property
                    {
                        // The param is directly property-loaded; the property is a dep
                        // This handles cases like function Foo({a, b}) where a, b come from
                        // Destructure but are used via PropertyLoad of the temp param
                        let _ = prop; // We handle this via derived_var_names
                    }
                }
            }

            if derived_var_names.is_empty() {
                continue;
            }

            // Now for each derived variable, trace property chains and find leaf uses
            // (same algorithm as find_reactive_deps for named params)
            for name in &derived_var_names {
                let mut var_load_ids: HashSet<IdentifierId> = HashSet::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        if let InstructionValue::LoadLocal { place, .. } = &instr.value
                            && let Some(IdentifierName::Named(n))
                            | Some(IdentifierName::Promoted(n)) = &place.identifier.name
                            && n == name
                        {
                            var_load_ids.insert(instr.lvalue.identifier.id);
                        }
                    }
                }

                if var_load_ids.is_empty() {
                    continue;
                }

                // Trace PropertyLoad chains from this variable
                let mut id_to_path: HashMap<IdentifierId, String> = HashMap::new();
                for &vid in &var_load_ids {
                    id_to_path.insert(vid, name.clone());
                }

                let mut changed = true;
                while changed {
                    changed = false;
                    for (_, block) in &func.body.blocks {
                        for instr in &block.instructions {
                            if let InstructionValue::PropertyLoad {
                                object,
                                property,
                                optional,
                                ..
                            } = &instr.value
                            {
                                if id_to_path.contains_key(&instr.lvalue.identifier.id) {
                                    continue;
                                }
                                if let Some(base_path) = id_to_path.get(&object.identifier.id) {
                                    let prop_str = match property {
                                        PropertyLiteral::String(s) => s.clone(),
                                        PropertyLiteral::Number(n) => n.to_string(),
                                    };
                                    let sep = if *optional { "?." } else { "." };
                                    let full_path = format!("{}{}{}", base_path, sep, prop_str);
                                    id_to_path.insert(instr.lvalue.identifier.id, full_path);
                                    changed = true;
                                }
                            }
                        }
                    }
                }

                // Find leaf uses (non-PropertyLoad consumers of chain IDs)
                let mut direct_use = false;
                let mut leaf_paths: Vec<String> = Vec::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        match &instr.value {
                            InstructionValue::PropertyLoad { .. } => {}
                            _ => {
                                crate::hir::visitors::for_each_instruction_operand(
                                    instr,
                                    |place| {
                                        if let Some(path) = id_to_path.get(&place.identifier.id) {
                                            if var_load_ids.contains(&place.identifier.id) {
                                                direct_use = true;
                                            } else if !leaf_paths.contains(path) {
                                                leaf_paths.push(path.clone());
                                            }
                                        }
                                    },
                                );
                            }
                        }
                    }
                    // Check terminal places
                    let mut check_terminal = |place: &Place| {
                        if let Some(path) = id_to_path.get(&place.identifier.id) {
                            if var_load_ids.contains(&place.identifier.id) {
                                direct_use = true;
                            } else if !leaf_paths.contains(path) {
                                leaf_paths.push(path.clone());
                            }
                        }
                    };
                    match &block.terminal {
                        Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                            check_terminal(value)
                        }
                        Terminal::If { test, .. }
                        | Terminal::Branch { test, .. }
                        | Terminal::Switch { test, .. } => check_terminal(test),
                        _ => {}
                    }
                }

                // Deduplicate leaf paths (remove sub-paths if parent exists)
                leaf_paths.sort();
                let mut deduped: Vec<String> = Vec::new();
                for path in &leaf_paths {
                    let has_prefix = deduped.iter().any(|existing| {
                        path.starts_with(existing.as_str())
                            && path.as_bytes().get(existing.len()) == Some(&b'.')
                    });
                    if !has_prefix {
                        deduped.retain(|existing| {
                            !(existing.starts_with(path.as_str())
                                && existing.as_bytes().get(path.len()) == Some(&b'.'))
                        });
                        deduped.push(path.clone());
                    }
                }

                if direct_use || deduped.is_empty() {
                    all_deps.push(name.clone());
                } else {
                    deduped.sort();
                    all_deps.extend(deduped);
                }
            }
        }

        // Sort for deterministic output
        all_deps.sort();
        all_deps.dedup();
        all_deps
    }

    /// Check if the function body is trivial (doesn't need memoization).
    /// A body is trivial if it doesn't create any allocations (objects, arrays, JSX,
    /// functions) — only primitives, parameter accesses, and simple operations.
    fn is_trivial_body(
        &self,
        func: &HIRFunction,
        memo_stmts: &[String],
        return_expr: &Option<String>,
    ) -> bool {
        // If there are no memo statements and no return, the body is trivially empty
        if memo_stmts.is_empty() && return_expr.is_none() {
            return true;
        }

        // Check if any instruction in the HIR creates an allocation
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    // Allocating instructions — not trivial
                    InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. }
                    | InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. }
                    | InstructionValue::ObjectMethod { .. }
                    | InstructionValue::RegExpLiteral { .. }
                    | InstructionValue::NewExpression { .. }
                    | InstructionValue::TaggedTemplateExpression { .. } => {
                        return false;
                    }
                    // Function expressions that are outlined don't count as allocations
                    // — after outlining they become a reference to the top-level name.
                    // Non-outlined function expressions ARE allocations.
                    InstructionValue::FunctionExpression { .. } => {
                        if !self.outlined_map.contains_key(&instr.lvalue.identifier.id) {
                            return false;
                        }
                    }
                    // Non-hook function calls might create objects — not trivial
                    InstructionValue::CallExpression { callee, .. } => {
                        let callee_str = self.resolve_place(callee);
                        if !self.looks_like_hook_call(&format!("{}()", callee_str)) {
                            return false;
                        }
                    }
                    InstructionValue::MethodCall { .. } => {
                        return false;
                    }
                    _ => {}
                }
            }
        }

        true
    }

    /// Check if the function's return value is known to produce a primitive,
    /// even if the body contains non-primitive-producing instructions like function calls.
    /// This traces the return place backward through the HIR to determine if the
    /// last instruction that defines the return value produces a primitive.
    fn returns_primitive_value(&self, func: &HIRFunction) -> bool {
        use std::collections::HashSet;

        // Find ALL return terminals' places — if ANY return is non-primitive,
        // the function is not purely primitive-returning.
        let return_place_ids: Vec<IdentifierId> = func
            .body
            .blocks
            .iter()
            .filter_map(|(_, block)| {
                if let Terminal::Return { value, .. } = &block.terminal {
                    Some(value.identifier.id)
                } else {
                    None
                }
            })
            .collect();

        if return_place_ids.is_empty() {
            return false;
        }

        // ALL return paths must produce primitives for this to be true
        for return_id in return_place_ids {
            let mut visited: HashSet<IdentifierId> = HashSet::new();
            if !self.id_produces_primitive(return_id, func, &mut visited) {
                return false;
            }
        }
        true
    }

    /// Check if the value produced by the instruction defining `id` is a primitive.
    fn id_produces_primitive(
        &self,
        id: IdentifierId,
        func: &HIRFunction,
        visited: &mut HashSet<IdentifierId>,
    ) -> bool {
        if !visited.insert(id) {
            return false; // cycle — conservatively not primitive
        }

        // Find the instruction that defines this identifier
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if instr.lvalue.identifier.id == id {
                    return self.instruction_produces_primitive_safe(&instr.value, func, visited);
                }
            }
        }

        false
    }

    /// Check if an instruction is known to produce a primitive value.
    fn instruction_produces_primitive_safe(
        &self,
        value: &InstructionValue,
        func: &HIRFunction,
        visited: &mut HashSet<IdentifierId>,
    ) -> bool {
        match value {
            // Primitives are... primitive
            InstructionValue::Primitive { .. } => true,
            // Binary/unary ops produce primitives
            InstructionValue::BinaryExpression { .. }
            | InstructionValue::UnaryExpression { .. } => true,
            // delete always returns boolean
            InstructionValue::PropertyDelete { .. } | InstructionValue::ComputedDelete { .. } => {
                true
            }
            // Template literals produce strings
            InstructionValue::TemplateLiteral { .. } => true,
            // LoadLocal: trace through to what it loads
            InstructionValue::LoadLocal { place, .. } => {
                // Find ALL StoreLocal instructions that write to this declaration.
                // For phi variables (multiple assignments in different branches),
                // ALL must produce primitives for the result to be considered primitive.
                let mut source_ids: Vec<IdentifierId> = Vec::new();
                for (_, block) in &func.body.blocks {
                    for instr in &block.instructions {
                        if let InstructionValue::StoreLocal {
                            lvalue,
                            value: store_val,
                            ..
                        } = &instr.value
                            && lvalue.place.identifier.declaration_id
                                == place.identifier.declaration_id
                        {
                            source_ids.push(store_val.identifier.id);
                        }
                    }
                }
                if source_ids.is_empty() {
                    false
                } else {
                    source_ids
                        .iter()
                        .all(|sid| self.id_produces_primitive(*sid, func, visited))
                }
            }
            // Property loads MIGHT produce primitives but we can't know without type info
            InstructionValue::PropertyLoad { .. } | InstructionValue::ComputedLoad { .. } => false,
            // Everything else is unknown or non-primitive
            _ => false,
        }
    }

    /// Check if any allocation in the function body escapes to the return value
    /// or to an external (non-hook) function call. If no allocation escapes,
    /// memoization provides no benefit — this mirrors the upstream compiler's
    /// `pruneNonEscapingScopes` pass.
    ///
    /// An "allocation" is an instruction that creates a heap value: ArrayExpression,
    /// ObjectExpression, JsxExpression, FunctionExpression, NewExpression, etc.
    ///
    /// An allocation "escapes" if its result identifier transitively flows to:
    /// 1. The return value of the function
    /// 2. An argument of a non-hook function call
    /// 3. A property store on a non-local object
    fn allocations_escape(&self, func: &HIRFunction) -> bool {
        use std::collections::HashSet;
        use std::collections::VecDeque;

        // Step 1: Find all allocation instruction lvalue IDs.
        // Includes explicit allocations AND non-hook function calls (which might
        // return objects that need memoization).
        let mut allocation_ids: HashSet<IdentifierId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                let is_alloc = match &instr.value {
                    InstructionValue::ObjectExpression { .. }
                    | InstructionValue::ArrayExpression { .. }
                    | InstructionValue::JsxExpression { .. }
                    | InstructionValue::JsxFragment { .. }
                    | InstructionValue::ObjectMethod { .. }
                    | InstructionValue::RegExpLiteral { .. }
                    | InstructionValue::NewExpression { .. }
                    | InstructionValue::TaggedTemplateExpression { .. } => true,
                    InstructionValue::FunctionExpression { .. } => {
                        !self.outlined_map.contains_key(&instr.lvalue.identifier.id)
                    }
                    // Non-hook function calls might return objects/arrays
                    InstructionValue::CallExpression { callee, .. } => {
                        let callee_str = self.resolve_place(callee);
                        !self.looks_like_hook_call(&format!("{}()", callee_str))
                    }
                    InstructionValue::MethodCall { .. } => true,
                    _ => false,
                };
                if is_alloc {
                    allocation_ids.insert(instr.lvalue.identifier.id);
                }
            }
        }

        if allocation_ids.is_empty() {
            return false;
        }

        // Step 2: Build a forward dataflow map: source_id → set of ids that receive it
        // via StoreLocal/LoadLocal chains.
        // Also build a map from declaration_id to all identifier_ids that store to it.
        let mut decl_to_stores: HashMap<DeclarationId, Vec<IdentifierId>> = HashMap::new();
        let mut decl_to_loads: HashMap<DeclarationId, Vec<IdentifierId>> = HashMap::new();

        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        decl_to_stores
                            .entry(lvalue.place.identifier.declaration_id)
                            .or_default()
                            .push(value.identifier.id);
                        // The store result (instr.lvalue) also carries the stored value
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        decl_to_loads
                            .entry(place.identifier.declaration_id)
                            .or_default()
                            .push(instr.lvalue.identifier.id);
                    }
                    _ => {}
                }
            }
        }

        // Step 3: Propagate allocation taint through StoreLocal→LoadLocal chains
        let mut tainted: HashSet<IdentifierId> = allocation_ids.clone();
        let mut queue: VecDeque<IdentifierId> = allocation_ids.iter().copied().collect();

        // Also build id → declaration_id map for store values
        let mut id_to_store_decl: HashMap<IdentifierId, Vec<DeclarationId>> = HashMap::new();
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, value, .. } = &instr.value {
                    id_to_store_decl
                        .entry(value.identifier.id)
                        .or_default()
                        .push(lvalue.place.identifier.declaration_id);
                }
            }
        }

        while let Some(id) = queue.pop_front() {
            // If this id is stored to a declaration, all loads from that declaration get tainted
            if let Some(decls) = id_to_store_decl.get(&id) {
                for decl in decls {
                    if let Some(load_ids) = decl_to_loads.get(decl) {
                        for &load_id in load_ids {
                            if tainted.insert(load_id) {
                                queue.push_back(load_id);
                            }
                        }
                    }
                }
            }
            // Propagate taint through all instructions that USE this id as an operand.
            // If any input place of an instruction is tainted, the output (lvalue) is tainted.
            for (_, block) in &func.body.blocks {
                for instr in &block.instructions {
                    let uses_tainted = instruction_input_place_ids(&instr.value).contains(&id);
                    if uses_tainted && tainted.insert(instr.lvalue.identifier.id) {
                        queue.push_back(instr.lvalue.identifier.id);
                    }
                }
            }
        }

        // Step 4: Check if any tainted value reaches an escape point
        // Escape point 1: Return value
        for (_, block) in &func.body.blocks {
            if let Terminal::Return { value, .. } = &block.terminal
                && tainted.contains(&value.identifier.id)
            {
                return true;
            }
        }

        // Escape point 2: Argument to any function call (including hooks).
        // Allocations passed to hooks (useEffect callbacks, useMemo args, etc.)
        // need memoization to maintain referential equality across renders.
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::CallExpression { args, .. } => {
                        for arg in args {
                            match arg {
                                Argument::Spread(p) | Argument::Place(p) => {
                                    if tainted.contains(&p.identifier.id) {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                    InstructionValue::MethodCall { args, .. } => {
                        for arg in args {
                            match arg {
                                Argument::Spread(p) | Argument::Place(p) => {
                                    if tainted.contains(&p.identifier.id) {
                                        return true;
                                    }
                                }
                            }
                        }
                    }
                    // Escape point 3: Property store on a non-local
                    InstructionValue::PropertyStore { value, .. }
                    | InstructionValue::ComputedStore { value, .. } => {
                        if tainted.contains(&value.identifier.id) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
        }

        // No allocation escapes
        false
    }

    /// Check if a function expression can be outlined (extracted to a top-level declaration).
    /// A function can be outlined if:
    /// 1. It has no name (anonymous)
    /// 2. Its body doesn't reference any variables from the outer scope (no captures)
    fn can_outline_function(&self, lowered_func: &LoweredFunction) -> bool {
        // Must be unnamed (upstream only outlines unnamed functions)
        if lowered_func.func.id.is_some() {
            return false;
        }
        !self.function_captures_outer_names(&lowered_func.func)
    }

    /// Check whether a function body (including nested functions) captures any outer scope names.
    fn function_captures_outer_names(&self, inner_func: &HIRFunction) -> bool {
        // Collect inner function's own parameter names
        let mut inner_param_names: HashSet<String> = HashSet::new();
        for param in &inner_func.params {
            if let Argument::Place(p) = param
                && let Some(IdentifierName::Named(n)) = &p.identifier.name
            {
                inner_param_names.insert(n.clone());
            }
        }
        // Collect all names that are DEFINED within the inner function via StoreLocal.
        let mut inner_defined_names: HashSet<String> = HashSet::new();
        for (_, block) in &inner_func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value {
                    match lvalue.kind {
                        InstructionKind::Let | InstructionKind::Const => {
                            if let Some(IdentifierName::Named(n)) = &lvalue.place.identifier.name {
                                inner_defined_names.insert(n.clone());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Collect all names REFERENCED within the inner function via LoadLocal, LoadGlobal,
        // or StoreLocal (assignment to outer variable is also a capture).
        let mut inner_loaded_names: HashSet<String> = HashSet::new();
        for (_, block) in &inner_func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::LoadLocal { place, .. } => {
                        if let Some(IdentifierName::Named(n)) = &place.identifier.name {
                            inner_loaded_names.insert(n.clone());
                        }
                    }
                    InstructionValue::LoadGlobal { binding, .. } => {
                        let name = match binding {
                            NonLocalBinding::Global { name } => Some(name),
                            NonLocalBinding::ModuleLocal { name } => Some(name),
                            _ => None,
                        };
                        if let Some(n) = name {
                            inner_loaded_names.insert(n.clone());
                        }
                    }
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        // Reassignment (not Let/Const) to an outer variable is a capture
                        if lvalue.kind == InstructionKind::Reassign
                            && let Some(IdentifierName::Named(n)) = &lvalue.place.identifier.name
                        {
                            inner_loaded_names.insert(n.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        // Check for captures: a name LOADED but NOT DEFINED/PARAM'd AND IS an outer scope name.
        for name in &inner_loaded_names {
            if !inner_param_names.contains(name)
                && !inner_defined_names.contains(name)
                && self.outer_scope_names.contains(name)
            {
                return true; // Has a capture
            }
        }
        // Recursively check nested function expressions
        for (_, block) in &inner_func.body.blocks {
            for instr in &block.instructions {
                if let InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value
                    && self.function_captures_outer_names(&lowered_func.func)
                {
                    return true;
                }
            }
        }
        false
    }

    /// Generate a unique outlined function name (_temp, _temp2, _temp3, ...).
    fn next_outline_name(&mut self) -> String {
        self.outline_counter += 1;
        if self.outline_counter == 1 {
            "_temp".to_string()
        } else {
            format!("_temp{}", self.outline_counter)
        }
    }

    /// Check if an instruction should go after the scope (reads from the promoted variable).
    fn is_post_scope_instruction(&self, instr: &Instruction, promoted_var: Option<&str>) -> bool {
        let Some(promoted) = promoted_var else {
            return false;
        };

        match &instr.value {
            InstructionValue::StoreLocal { value, lvalue, .. } => {
                // If this stores into the promoted variable, it's IN the scope
                let name = match &lvalue.place.identifier.name {
                    Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                        n.as_str()
                    }
                    None => return false,
                };
                if name == promoted {
                    return false;
                }

                // If the value reads from the promoted variable (via expr_map), it's post-scope
                if let Some(expr) = self.expr_map.get(&value.identifier.id)
                    && expr.starts_with(promoted)
                    && expr.len() > promoted.len()
                {
                    let next_char = expr.as_bytes()[promoted.len()];
                    if next_char == b'.' || next_char == b'[' {
                        return true;
                    }
                }
                false
            }
            InstructionValue::PropertyStore { object, .. } => {
                // PropertyStore on the promoted var is IN the scope (mutation)
                if self.resolve_to_name(object) == Some(promoted.to_string()) {
                    return false;
                }
                false
            }
            InstructionValue::CallExpression { args, .. } => {
                // If any argument references the promoted variable, this call
                // reads from the scope output and should be post-scope.
                // This handles patterns like `console.log(x)` or `useFreeze(a)`.
                for arg in args {
                    let place = match arg {
                        Argument::Place(p) | Argument::Spread(p) => p,
                    };
                    if self.resolve_to_name(place) == Some(promoted.to_string()) {
                        return true;
                    }
                    if let Some(expr) = self.expr_map.get(&place.identifier.id)
                        && expr == promoted
                    {
                        return true;
                    }
                }
                false
            }
            InstructionValue::MethodCall { args, receiver, .. } => {
                // Method calls that take the promoted var as an arg are post-scope reads
                for arg in args {
                    let place = match arg {
                        Argument::Place(p) | Argument::Spread(p) => p,
                    };
                    if self.resolve_to_name(place) == Some(promoted.to_string()) {
                        return true;
                    }
                    if let Some(expr) = self.expr_map.get(&place.identifier.id)
                        && expr == promoted
                    {
                        return true;
                    }
                }
                // Method calls ON the promoted var are mutations (in-scope), not reads
                if self.resolve_to_name(receiver) == Some(promoted.to_string()) {
                    return false;
                }
                false
            }
            _ => false,
        }
    }

    /// Check if an instruction is a known-readonly call that consumes the promoted variable.
    /// More conservative than is_post_scope_instruction: only matches calls to known
    /// non-mutating functions like console.log, console.info, etc.
    fn is_readonly_post_scope_call(&self, instr: &Instruction, promoted_var: Option<&str>) -> bool {
        let Some(promoted) = promoted_var else {
            return false;
        };

        // Check for side-effect-only calls (calls whose result is unused)
        match &instr.value {
            InstructionValue::CallExpression { callee, args, .. } => {
                // Check if callee is a known readonly function
                let callee_expr = self
                    .expr_map
                    .get(&callee.identifier.id)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                let is_readonly_callee = callee_expr.starts_with("console.")
                    || callee_expr.starts_with("global.console.")
                    || callee_expr == "identity";

                if !is_readonly_callee {
                    return false;
                }

                // Check if any argument references the promoted variable
                for arg in args {
                    let place = match arg {
                        Argument::Place(p) | Argument::Spread(p) => p,
                    };
                    if self.resolve_to_name(place) == Some(promoted.to_string()) {
                        return true;
                    }
                    if let Some(expr) = self.expr_map.get(&place.identifier.id)
                        && expr == promoted
                    {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    /// Check if all arguments to a call are "prescope" — i.e., available before
    /// the memo block. An argument is prescope if it is a parameter, a global, or
    /// the call has no arguments at all. This determines whether a side-effect-only
    /// call goes before the memo block (pre_memo) or after it (post_scope).
    fn call_args_are_prescope(
        &self,
        instr: &Instruction,
        param_ids: &HashSet<IdentifierId>,
        global_ids: &HashSet<IdentifierId>,
    ) -> bool {
        let args = match &instr.value {
            InstructionValue::CallExpression { args, .. }
            | InstructionValue::MethodCall { args, .. } => args,
            _ => return true,
        };
        if args.is_empty() {
            return true;
        }
        for arg in args {
            let id = match arg {
                Argument::Place(p) | Argument::Spread(p) => p.identifier.id,
            };
            // Direct param or global id — prescope
            if param_ids.contains(&id) || global_ids.contains(&id) {
                continue;
            }
            // Check expression: if it looks like a param name or literal, prescope
            if let Some(expr) = self.expr_map.get(&id)
                && is_literal_primitive(expr)
            {
                continue;
            }
            // Argument depends on scope-computed value
            return false;
        }
        true
    }
    /// Check if an instruction is a hook call statement (should be placed before memo scope).
    fn is_hook_call_stmt(&self, instr: &Instruction) -> bool {
        // A StoreLocal whose value is a hook call result
        if let InstructionValue::StoreLocal { value, .. } = &instr.value
            && let Some(expr) = self.expr_map.get(&value.identifier.id)
        {
            return self.looks_like_hook_call(expr);
        }
        // A Destructure whose value is a hook call result (e.g., `const [, setX] = useState(0)`)
        if let InstructionValue::Destructure { value, .. } = &instr.value
            && let Some(expr) = self.expr_map.get(&value.identifier.id)
        {
            return self.looks_like_hook_call(expr);
        }
        // A standalone CallExpression that is a hook call
        if let InstructionValue::CallExpression { callee, .. } = &instr.value {
            let callee_str = self.resolve_place(callee);
            if self.looks_like_hook_call(&format!("{}()", callee_str)) {
                return true;
            }
        }
        false
    }

    /// Check if a call instruction is a side-effect-only call (fire-and-forget) to a
    /// global receiver/callee. Such calls should run on every render (outside the memo
    /// block), not be cached.
    ///
    /// This handles patterns like `console.log(x)` — the call reads from reactive values
    /// but doesn't produce a meaningful result and doesn't mutate scoped values.
    /// Local function calls like `x()` are NOT moved outside because they may mutate
    /// scoped values through closures.
    fn is_side_effect_only_call(
        &self,
        instr: &Instruction,
        global_ids: &HashSet<IdentifierId>,
    ) -> bool {
        match &instr.value {
            InstructionValue::MethodCall { receiver, .. } => {
                // Only move outside if receiver was loaded from a global (e.g., console)
                global_ids.contains(&receiver.identifier.id)
            }
            InstructionValue::CallExpression { callee, .. } => {
                // Only move outside if callee was loaded from a global
                global_ids.contains(&callee.identifier.id)
            }
            _ => false,
        }
    }

    /// Check if an instruction should be placed before the scope guard.
    /// An instruction is pre-scope if:
    /// 1. Its instruction ID is before the scope range start, AND
    /// 2. All its operands are either params, globals, or outputs of other pre-scope instructions
    ///
    /// This ensures non-hook statements that don't depend on the scope are placed
    /// before the scope guard, maintaining their original order relative to hooks.
    fn is_pre_scope_instruction(
        &self,
        instr: &Instruction,
        scope_range: &Option<(InstructionId, InstructionId)>,
        pre_scope_instr_ids: &HashSet<InstructionId>,
        param_ids: &HashSet<IdentifierId>,
        global_ids: &HashSet<IdentifierId>,
    ) -> bool {
        // If no scope range, nothing is "before the scope"
        let (scope_start, _scope_end) = match scope_range {
            Some(r) => r,
            None => return false,
        };

        // Only instructions before the scope range start
        if instr.id >= *scope_start {
            return false;
        }

        // Don't hoist DeclareLocal/StoreLocal unless they produce values consumed
        // by other pre-scope instructions. Simple declarations should stay in place
        // to avoid reordering user-visible side effects.
        match &instr.value {
            InstructionValue::DeclareLocal { .. }
            | InstructionValue::LoadLocal { .. }
            | InstructionValue::LoadContext { .. }
            | InstructionValue::Primitive { .. }
            | InstructionValue::LoadGlobal { .. } => {
                // These are data-flow instructions, not user-visible statements.
                // They can safely be pre-scope.
                return false; // But they don't generate statements, so no need to classify
            }
            _ => {}
        }

        // Check all operands: must be params, globals, or outputs of pre-scope instructions
        let mut all_prescope = true;
        crate::hir::visitors::for_each_instruction_operand(instr, |place| {
            let id = place.identifier.id;
            if !param_ids.contains(&id) && !global_ids.contains(&id) {
                // Check if it was produced by a pre-scope instruction
                // (use the expr_map to trace through LoadLocal chains)
                if !pre_scope_instr_ids.iter().any(|_| {
                    // Check if this identifier was produced by any pre-scope instruction
                    // This is an approximation — we check the identifier against known pre-scope outputs
                    false
                }) {
                    all_prescope = false;
                }
            }
        });
        all_prescope
    }

    /// Check if an instruction is a StoreLocal that stores a value from an outlined function
    /// AND the stored variable is not subsequently called within the function body.
    /// These assignments (e.g., `const foo = _temp;`) should go before the scope
    /// because outlined functions are module-level constants that don't depend on
    /// reactive state. However, if the variable IS called (its return value feeds
    /// into the scope computation), keep it inside the scope.
    fn is_outlined_store(&self, instr: &Instruction) -> bool {
        if let InstructionValue::StoreLocal { value, .. } = &instr.value {
            return self.outlined_map.contains_key(&value.identifier.id);
        }
        false
    }

    /// Check if an expression string looks like a hook call (starts with "use").
    fn looks_like_hook_call(&self, expr: &str) -> bool {
        // Match patterns like "useRef(...)", "useState(...)", "useMemo(...)" etc.
        let name = expr.split('(').next().unwrap_or("");
        name.starts_with("use")
            && name.len() > 3
            && name.chars().nth(3).is_some_and(|c| c.is_uppercase())
    }

    /// Check if a hook's direct return value is stable (not reactive).
    /// Stable hooks: useRef, useCallback (when properly memoized)
    fn is_stable_hook_return(hook_name: &str) -> bool {
        matches!(hook_name, "useRef")
    }

    /// Check if a destructured element at the given index is reactive.
    /// For useState/useReducer: index 0 (state) is reactive, index 1 (setter/dispatch) is stable.
    /// For useActionState: index 0 (state) is reactive, index 1 (dispatch) is stable, index 2 (pending) is reactive.
    fn is_reactive_destructured_element(hook_name: &str, index: usize) -> bool {
        match hook_name {
            "useState" | "useReducer" => index == 0, // Only state is reactive
            "useActionState" => index == 0 || index == 2, // state and pending are reactive
            _ => true, // Conservative: all elements are reactive for unknown hooks
        }
    }

    /// Convert an instruction to a statement string (if it produces visible code).
    /// Returns None for instructions that are just temporaries.
    fn instruction_to_stmt(&self, instr: &Instruction) -> Option<String> {
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, value, .. } => {
                // Skip stores that are part of param destructuring default chains
                // (pipeline handles param defaults — HIR also generates them, causing duplicates)
                if self
                    .param_default_stores
                    .contains(&lvalue.place.identifier.id)
                {
                    return None;
                }
                let name = self.identifier_name(&lvalue.place.identifier);
                let val = self.resolve_place(value);

                // If this variable is promoted (declared as `let x;` before the scope),
                // use bare assignment instead of const/let declaration.
                let is_promoted = self
                    .promoted_store_ids
                    .contains(&lvalue.place.identifier.id);

                let keyword = if is_promoted {
                    ""
                } else {
                    match lvalue.kind {
                        InstructionKind::Const | InstructionKind::HoistedConst => "const",
                        InstructionKind::Let
                        | InstructionKind::HoistedLet
                        | InstructionKind::Catch => "let",
                        InstructionKind::Reassign => "",
                        InstructionKind::Function | InstructionKind::HoistedFunction => "",
                    }
                };
                if keyword.is_empty() {
                    Some(format!("{} = {};", name, val))
                } else if val == "undefined" && (keyword == "let" || keyword == "const") {
                    // `let/const y = undefined;` → `let y;`
                    // Upstream keeps these as uninitialized `let` declarations.
                    // Note: `const x;` is invalid JS, so we always use `let`.
                    // Skip if already emitted as a promoted var declaration.
                    if self.emitted_let_decls.contains(&name) {
                        return None;
                    }
                    Some(format!("let {};", name))
                } else {
                    Some(format!("{} {} = {};", keyword, name, val))
                }
            }
            InstructionValue::DeclareLocal { lvalue, .. } => {
                // DeclareLocal with Catch kind produces no output — the catch
                // binding is declared via the catch clause parameter syntax.
                if lvalue.kind == InstructionKind::Catch {
                    return None;
                }
                let name = self.identifier_name(&lvalue.place.identifier);
                // Skip if this name was already emitted as a promoted var declaration.
                if self.emitted_let_decls.contains(&name) {
                    return None;
                }
                // DeclareLocal emits `let x;` (no initializer).
                // Always use `let` — `const x;` is invalid JS.
                Some(format!("let {};", name))
            }
            InstructionValue::Destructure { lvalue, value, .. } => {
                // Skip param destructurings — pipeline handles them separately.
                // Only skip when the value comes from an anonymous/destructured param
                // (e.g., `function t({a, b})` → param replaced by temp, destructure handled by pipeline).
                // Body-level destructurings of named params (e.g., `let [, foo] = props;`) must NOT be skipped.
                if self
                    .destructured_param_load_ids
                    .contains(&value.identifier.id)
                {
                    return None;
                }
                let val = self.resolve_place(value);
                let keyword = match lvalue.kind {
                    InstructionKind::Const | InstructionKind::HoistedConst => "const",
                    InstructionKind::Let | InstructionKind::HoistedLet => "let",
                    InstructionKind::Reassign => "",
                    _ => "const",
                };
                let pattern_str = self.pattern_to_string(&lvalue.pattern);
                if keyword.is_empty() {
                    // Object destructuring reassignment needs parens to avoid ambiguity with block
                    if matches!(&lvalue.pattern, Pattern::Object(_)) {
                        Some(format!("({} = {});", pattern_str, val))
                    } else {
                        Some(format!("{} = {};", pattern_str, val))
                    }
                } else {
                    Some(format!("{} {} = {};", keyword, pattern_str, val))
                }
            }
            InstructionValue::CallExpression {
                callee,
                args,
                optional,
                ..
            } => {
                // Only emit as standalone if lvalue is an unused temporary
                // (not consumed by a StoreLocal)
                if instr.lvalue.identifier.name.is_none()
                    && !self.consumed_temps.contains(&instr.lvalue.identifier.id)
                {
                    let callee_str = self.resolve_place(callee);
                    let args_str = self.resolve_args(args);
                    if *optional {
                        Some(format!("{}?.({});", callee_str, args_str))
                    } else {
                        Some(format!("{}({});", callee_str, args_str))
                    }
                } else {
                    None // Will be emitted as part of StoreLocal or inlined
                }
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                receiver_optional,
                call_optional,
                ..
            } => {
                if instr.lvalue.identifier.name.is_none()
                    && !self.consumed_temps.contains(&instr.lvalue.identifier.id)
                {
                    let recv = self.resolve_place(receiver);
                    let (prop, is_computed) = self.resolve_method_property(property, &recv);
                    let args_str = self.resolve_args(args);
                    if is_computed {
                        let opt_recv = if *receiver_optional { "?." } else { "" };
                        if *call_optional {
                            Some(format!("{}{}[{}]?.({});", recv, opt_recv, prop, args_str))
                        } else {
                            Some(format!("{}{}[{}]({});", recv, opt_recv, prop, args_str))
                        }
                    } else {
                        let dot = if *receiver_optional { "?." } else { "." };
                        if *call_optional {
                            Some(format!("{}{}{}?.({});", recv, dot, prop, args_str))
                        } else {
                            Some(format!("{}{}{}({});", recv, dot, prop, args_str))
                        }
                    }
                } else {
                    None
                }
            }
            InstructionValue::NewExpression { callee, args, .. } => {
                if instr.lvalue.identifier.name.is_none()
                    && !self.consumed_temps.contains(&instr.lvalue.identifier.id)
                {
                    let callee_str = self.resolve_place(callee);
                    let args_str = self.resolve_args(args);
                    Some(format!("new {}({});", callee_str, args_str))
                } else {
                    None
                }
            }
            InstructionValue::PropertyStore {
                object,
                property,
                value,
                ..
            } => {
                let obj = self.resolve_place(object);
                let val = self.resolve_place(value);
                match property {
                    PropertyLiteral::String(s) => Some(format!("{}.{} = {};", obj, s, val)),
                    PropertyLiteral::Number(n) => {
                        Some(format!("{}[{}] = {};", obj, format_number(*n), val))
                    }
                }
            }
            InstructionValue::ComputedStore {
                object,
                property,
                value,
                ..
            } => {
                let obj = self.resolve_place(object);
                let prop = self.resolve_place(property);
                let val = self.resolve_place(value);
                // Use dot notation for string keys that are valid identifiers
                if prop.starts_with('"') && prop.ends_with('"') {
                    let inner = &prop[1..prop.len() - 1];
                    if is_valid_identifier(inner) {
                        return Some(format!("{}.{} = {};", obj, inner, val));
                    }
                }
                Some(format!("{}[{}] = {};", obj, prop, val))
            }
            InstructionValue::PropertyDelete {
                object, property, ..
            } => {
                // If this delete's result is consumed (e.g. `let y = delete x.prop`),
                // it will be inlined via expr_map — don't also emit standalone.
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                let obj = self.resolve_place(object);
                match property {
                    PropertyLiteral::String(s) => Some(format!("delete {}.{};", obj, s)),
                    PropertyLiteral::Number(n) => {
                        Some(format!("delete {}[{}];", obj, format_number(*n)))
                    }
                }
            }
            InstructionValue::ComputedDelete {
                object, property, ..
            } => {
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                let obj = self.resolve_place(object);
                let prop = self.resolve_place(property);
                Some(format!("delete {}[{}];", obj, prop))
            }
            InstructionValue::StoreGlobal { name, value, .. } => {
                let val = self.resolve_place(value);
                Some(format!("{} = {};", name, val))
            }
            InstructionValue::FunctionExpression {
                name: Some(fn_name),
                lowered_func,
                expr_type,
                ..
            } if *expr_type == FunctionExpressionType::FunctionDeclaration => {
                // If this temp is consumed by a StoreLocal, let StoreLocal handle emission.
                // This prevents duplicate output (function decl + variable assignment).
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                // Emit function declaration with body
                let inner_body = self.generate_inner_function_body(&lowered_func.func);
                let params = self.generate_inner_function_params(&lowered_func.func);
                if inner_body.is_empty() {
                    Some(format!("function {}({}) {{}}", fn_name, params))
                } else {
                    Some(format!(
                        "function {}({}) {{\n{}\n}}",
                        fn_name, params, inner_body
                    ))
                }
            }
            InstructionValue::Debugger { .. } => Some("debugger;".to_string()),
            InstructionValue::PrefixUpdate {
                value, operation, ..
            } => {
                // Standalone prefix update (e.g., `++i;` not assigned to anything)
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                let v = self.resolve_place(value);
                let op = match operation {
                    UpdateOperator::Increment => "++",
                    UpdateOperator::Decrement => "--",
                };
                Some(format!("{}{};", op, v))
            }
            InstructionValue::PostfixUpdate {
                value, operation, ..
            } => {
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                let v = self.resolve_place(value);
                let op = match operation {
                    UpdateOperator::Increment => "++",
                    UpdateOperator::Decrement => "--",
                };
                Some(format!("{}{};", v, op))
            }
            InstructionValue::LogicalExpression { .. } => {
                // Emit as standalone expression statement when the result is unused.
                // LogicalExpressions may have side effects (property reads, short-circuit evaluation)
                // and the upstream compiler preserves them even when the result isn't consumed.
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                self.instruction_to_expr(instr)
                    .map(|expr| format!("{};", expr))
            }
            InstructionValue::Ternary { .. } => {
                // Emit as standalone expression statement when the result is unused.
                // Ternary expressions may have side effects in their branches.
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                self.instruction_to_expr(instr)
                    .map(|expr| format!("{};", expr))
            }
            InstructionValue::TaggedTemplateExpression { .. } => {
                // Emit as standalone expression statement when the result is unused.
                // Tagged template expressions may have side effects.
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                self.instruction_to_expr(instr)
                    .map(|expr| format!("{};", expr))
            }
            InstructionValue::Await { value, .. } => {
                // Await expressions always have side effects — emit as standalone statement
                // even when the result is unused (e.g., `await sideEffect()`)
                if self.consumed_temps.contains(&instr.lvalue.identifier.id) {
                    return None;
                }
                let val = self.resolve_place(value);
                Some(format!("await {};", val))
            }
            _ => None, // Temporaries and other instructions don't emit standalone code
        }
    }

    /// Convert an instruction to an expression string (for the expr_map).
    fn instruction_to_expr(&self, instr: &Instruction) -> Option<String> {
        match &instr.value {
            InstructionValue::Primitive { value, .. } => Some(self.primitive_str(value)),
            InstructionValue::ArrayExpression { elements, .. } => {
                let elems: Vec<String> = elements
                    .iter()
                    .map(|e| match e {
                        ArrayElement::Place(p) => self.resolve_place(p),
                        ArrayElement::Spread(p) => format!("...{}", self.resolve_place(p)),
                        ArrayElement::Hole => String::new(),
                    })
                    .collect();
                Some(format!("[{}]", elems.join(", ")))
            }
            InstructionValue::ObjectExpression { properties, .. } => {
                let has_methods = properties.iter().any(|p| matches!(p, ObjectPropertyOrSpread::Property(prop) if prop.type_ == ObjectPropertyType::Method));
                let props: Vec<String> = properties
                    .iter()
                    .map(|p| match p {
                        ObjectPropertyOrSpread::Property(prop) => {
                            let key = match &prop.key {
                                ObjectPropertyKey::Identifier(s) => s.clone(),
                                ObjectPropertyKey::String(s) => {
                                    if is_valid_identifier(s) {
                                        s.clone()
                                    } else {
                                        format!("\"{}\"", escape_js_string(s))
                                    }
                                }
                                ObjectPropertyKey::Number(n) => format_number(*n),
                                ObjectPropertyKey::Computed(p) => {
                                    format!("[{}]", self.resolve_place(p))
                                }
                            };
                            // For method properties, emit as method shorthand: key(params) { body }
                            if prop.type_ == ObjectPropertyType::Method {
                                let val = self.resolve_place(&prop.place);
                                // The val should be a function expression string like "function name(params) { body }"
                                // Convert to method shorthand: key(params) { body }
                                let method_result = if let Some(rest) = val.strip_prefix("function")
                                {
                                    // rest is like " name(params) { body }" or " (params) { body }"
                                    let rest = rest.trim_start();
                                    // Skip the function name if present (find the opening paren)
                                    if let Some(paren_pos) = rest.find('(') {
                                        let params_and_body = &rest[paren_pos..];
                                        Some(format!("{}{}", key, params_and_body))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };
                                method_result.unwrap_or_else(|| format!("{}: {}", key, val))
                            } else {
                                let val = self.resolve_place(&prop.place);
                                // Use shorthand when key matches value (e.g., { x } instead of { x: x })
                                if key == val {
                                    key
                                } else {
                                    format!("{}: {}", key, val)
                                }
                            }
                        }
                        ObjectPropertyOrSpread::Spread(p) => {
                            format!("...{}", self.resolve_place(p))
                        }
                    })
                    .collect();
                // Add spaces inside braces: { x } instead of {x}
                if props.is_empty() {
                    Some("{}".to_string())
                } else if has_methods {
                    // Multiline format for objects with methods to match upstream output
                    let mut lines = String::from("{\n");
                    for prop_str in &props {
                        let prop_lines: Vec<&str> = prop_str.lines().collect();
                        if prop_lines.len() == 1 {
                            // Single-line property
                            lines.push_str("  ");
                            lines.push_str(prop_str);
                            lines.push_str(",\n");
                        } else {
                            // Multi-line method
                            for (i, line) in prop_lines.iter().enumerate() {
                                if i == prop_lines.len() - 1 && line.trim() == "}" {
                                    // Last line closing brace: indent once, add trailing comma
                                    lines.push_str("  },\n");
                                } else if i == 0 {
                                    // First line: "method() {"
                                    lines.push_str("  ");
                                    lines.push_str(line);
                                    lines.push('\n');
                                } else {
                                    // Inner body lines: indent twice
                                    lines.push_str("    ");
                                    lines.push_str(line.trim());
                                    lines.push('\n');
                                }
                            }
                        }
                    }
                    lines.push('}');
                    Some(lines)
                } else {
                    Some(format!("{{ {} }}", props.join(", ")))
                }
            }
            InstructionValue::CallExpression {
                callee,
                args,
                optional,
                ..
            } => {
                let callee_str = self.resolve_place(callee);
                let args_str = self.resolve_args(args);
                // Wrap callee in parens if it's a function expression to create
                // proper IIFE syntax: ((x) => ...)() instead of (x) => ...()
                let needs_wrap = callee_str.contains("=>") || callee_str.starts_with("function");
                let callee_final = if needs_wrap {
                    format!("({})", callee_str)
                } else {
                    callee_str
                };
                if *optional {
                    Some(format!("{}?.({})", callee_final, args_str))
                } else {
                    Some(format!("{}({})", callee_final, args_str))
                }
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                receiver_optional,
                call_optional,
                ..
            } => {
                let recv = self.resolve_place(receiver);
                let (prop, is_computed) = self.resolve_method_property(property, &recv);
                let args_str = self.resolve_args(args);
                if is_computed {
                    let opt_recv = if *receiver_optional { "?." } else { "" };
                    if *call_optional {
                        Some(format!("{}{}[{}]?.({})", recv, opt_recv, prop, args_str))
                    } else {
                        Some(format!("{}{}[{}]({})", recv, opt_recv, prop, args_str))
                    }
                } else {
                    let dot = if *receiver_optional { "?." } else { "." };
                    if *call_optional {
                        Some(format!("{}{}{}?.({})", recv, dot, prop, args_str))
                    } else {
                        Some(format!("{}{}{}({})", recv, dot, prop, args_str))
                    }
                }
            }
            InstructionValue::BinaryExpression {
                operator,
                left,
                right,
                ..
            } => {
                let prec = binary_op_precedence(operator);
                let l = self.resolve_place_with_min_prec(left, prec);
                let r = self.resolve_place_with_min_prec(right, prec + 1);
                let op = binary_op_str(operator);
                Some(format!("{} {} {}", l, op, r))
            }
            InstructionValue::UnaryExpression {
                operator, value, ..
            } => {
                let v = self.resolve_place(value);
                let op = unary_op_str(operator);
                Some(format!("{}{}", op, v))
            }
            InstructionValue::LoadLocal { place, .. } => Some(self.resolve_place_inner(place)),
            InstructionValue::LoadGlobal { binding, .. } => match binding {
                NonLocalBinding::Global { name } => Some(name.clone()),
                NonLocalBinding::ImportDefault { name, .. }
                | NonLocalBinding::ImportNamespace { name, .. }
                | NonLocalBinding::ImportSpecifier { name, .. }
                | NonLocalBinding::ModuleLocal { name } => Some(name.clone()),
            },
            InstructionValue::PropertyLoad {
                object,
                property,
                optional,
                ..
            } => {
                let obj = self.resolve_place(object);
                match property {
                    PropertyLiteral::String(s) => {
                        let dot = if *optional { "?." } else { "." };
                        Some(format!("{}{}{}", obj, dot, s))
                    }
                    PropertyLiteral::Number(n) => {
                        let num = format_number(*n);
                        if *optional {
                            Some(format!("{}?.[{}]", obj, num))
                        } else {
                            Some(format!("{}[{}]", obj, num))
                        }
                    }
                }
            }
            InstructionValue::ComputedLoad {
                object,
                property,
                optional,
                ..
            } => {
                let obj = self.resolve_place(object);
                let prop = self.resolve_place(property);
                if *optional {
                    // If the property is a string literal that's a valid identifier,
                    // use ?. notation (e.g., x?.a)
                    if prop.starts_with('"') && prop.ends_with('"') {
                        let inner = &prop[1..prop.len() - 1];
                        if is_valid_identifier(inner) {
                            return Some(format!("{}?.{}", obj, inner));
                        }
                    }
                    Some(format!("{}?.[{}]", obj, prop))
                } else {
                    // If the property is a string literal that's a valid identifier,
                    // use dot notation instead of bracket notation (e.g., x.a instead of x["a"])
                    if prop.starts_with('"') && prop.ends_with('"') {
                        let inner = &prop[1..prop.len() - 1];
                        if is_valid_identifier(inner) {
                            return Some(format!("{}.{}", obj, inner));
                        }
                    }
                    Some(format!("{}[{}]", obj, prop))
                }
            }
            InstructionValue::JsxExpression {
                tag,
                props,
                children,
                ..
            } => Some(self.jsx_to_string(tag, props, children.as_deref())),
            InstructionValue::JsxFragment { children, .. } => {
                if children.is_empty() {
                    Some("<></>".to_string())
                } else {
                    let joined = self.join_jsx_children(children);
                    Some(format!("<>{}</>", joined))
                }
            }
            InstructionValue::TemplateLiteral {
                subexprs, quasis, ..
            } => {
                // Simplify: empty template literal `` → ""
                if subexprs.is_empty() && quasis.len() == 1 && quasis[0].raw.is_empty() {
                    return Some("\"\"".to_string());
                }
                // Simplify: template literal with no expressions → plain string
                if subexprs.is_empty() && quasis.len() == 1 {
                    let raw = &quasis[0].raw;
                    // Only simplify if it doesn't contain characters that need escaping differently
                    if !raw.contains('`') && !raw.contains("${") {
                        return Some(format!("\"{}\"", raw));
                    }
                }
                let mut result = String::from("`");
                for (i, quasi) in quasis.iter().enumerate() {
                    result.push_str(&quasi.raw);
                    if i < subexprs.len() {
                        result.push_str("${");
                        result.push_str(&self.resolve_place(&subexprs[i]));
                        result.push('}');
                    }
                }
                result.push('`');
                Some(result)
            }
            InstructionValue::NewExpression { callee, args, .. } => {
                let callee_str = self.resolve_place(callee);
                let args_str = self.resolve_args(args);
                Some(format!("new {}({})", callee_str, args_str))
            }
            InstructionValue::FunctionExpression {
                name,
                lowered_func,
                expr_type,
                ..
            } => {
                // Check if this function was outlined during analyze()
                if let Some(outline_name) = self.outlined_map.get(&instr.lvalue.identifier.id) {
                    return Some(outline_name.clone());
                }
                // Generate the inner function body by recursively codegen-ing
                let inner_body = self.generate_inner_function_body(&lowered_func.func);
                let params = self.generate_inner_function_params(&lowered_func.func);
                match expr_type {
                    FunctionExpressionType::ArrowFunctionExpression => {
                        let trimmed_body = inner_body.trim();
                        if inner_body.is_empty() || trimmed_body.is_empty() {
                            Some(format!("({}) => {{}}", params))
                        } else if trimmed_body.starts_with("return ") {
                            // Check if body is a single return statement (possibly multi-line)
                            let expr = trimmed_body.trim_start_matches("return ");
                            let expr = expr.trim_end_matches(';').trim_end();
                            if !expr.contains('\n') {
                                // Single-line concise: () => expr
                                // Wrap in parens if the expression is a ternary — it has
                                // lower precedence than => and needs explicit grouping
                                let needs_parens = expr.contains(" ? ") && expr.contains(" : ");
                                if needs_parens {
                                    Some(format!("({}) => ({})", params, expr))
                                } else {
                                    Some(format!("({}) => {}", params, expr))
                                }
                            } else {
                                // Multi-line expression (e.g., JSX): () => (\nexpr\n)
                                Some(format!("({}) => (\n{}\n)", params, expr))
                            }
                        } else {
                            Some(format!("({}) => {{\n{}\n}}", params, inner_body))
                        }
                    }
                    FunctionExpressionType::FunctionExpression
                    | FunctionExpressionType::FunctionDeclaration => {
                        let fn_name = name.as_deref().unwrap_or("");
                        if inner_body.is_empty() {
                            Some(format!("function {}({}) {{}}", fn_name, params))
                        } else {
                            Some(format!(
                                "function {}({}) {{\n{}\n}}",
                                fn_name, params, inner_body
                            ))
                        }
                    }
                }
            }
            InstructionValue::StoreLocal { value, .. } => {
                // StoreLocal's expr is just the value
                Some(self.resolve_place(value))
            }
            InstructionValue::JSXText { value, .. } => Some(value.clone()),
            InstructionValue::RegExpLiteral { pattern, flags, .. } => {
                Some(format!("/{}/{}", pattern, flags))
            }
            InstructionValue::Await { value, .. } => {
                Some(format!("await {}", self.resolve_place(value)))
            }
            InstructionValue::PrefixUpdate {
                value, operation, ..
            } => {
                let v = self.resolve_place(value);
                let op = match operation {
                    UpdateOperator::Increment => "++",
                    UpdateOperator::Decrement => "--",
                };
                Some(format!("{}{}", op, v))
            }
            InstructionValue::PostfixUpdate {
                value, operation, ..
            } => {
                let v = self.resolve_place(value);
                let op = match operation {
                    UpdateOperator::Increment => "++",
                    UpdateOperator::Decrement => "--",
                };
                Some(format!("{}{}", v, op))
            }
            InstructionValue::MetaProperty { meta, property, .. } => {
                Some(format!("{}.{}", meta, property))
            }
            InstructionValue::PropertyDelete {
                object, property, ..
            } => {
                let obj = self.resolve_place(object);
                match property {
                    PropertyLiteral::String(s) => Some(format!("delete {}.{}", obj, s)),
                    PropertyLiteral::Number(n) => {
                        Some(format!("delete {}[{}]", obj, format_number(*n)))
                    }
                }
            }
            InstructionValue::ComputedDelete {
                object, property, ..
            } => {
                let obj = self.resolve_place(object);
                let prop = self.resolve_place(property);
                Some(format!("delete {}[{}]", obj, prop))
            }
            InstructionValue::TaggedTemplateExpression { tag, raw, .. } => {
                let tag_str = self.resolve_place(tag);
                Some(format!("{}`{}`", tag_str, raw))
            }
            InstructionValue::Ternary {
                test,
                consequent,
                alternate,
                ..
            } => {
                let t = self.resolve_place(test);
                // Wrap consequent if it contains a ternary or ?? (for clarity/unambiguity)
                let c = self.resolve_place_with_min_prec(consequent, 5);
                // Wrap alternate if it's ?? (spec requirement) or ternary
                let a = self.resolve_place_with_min_prec(alternate, 5);
                Some(format!("{} ? {} : {}", t, c, a))
            }
            InstructionValue::LogicalExpression {
                operator,
                left,
                right,
                ..
            } => {
                let prec = logical_op_precedence(operator);
                // Upstream always parenthesizes when mixing logical operators (||/&&/??)
                // for clarity. Wrap any child with a different logical precedence.
                let l = self.resolve_place_with_logical_parens(left, prec);
                let r = self.resolve_place_with_logical_parens(right, prec);
                let op = match operator {
                    LogicalOperator::And => "&&",
                    LogicalOperator::Or => "||",
                    LogicalOperator::NullishCoalescing => "??",
                };
                Some(format!("{} {} {}", l, op, r))
            }
            InstructionValue::TypeCastExpression {
                value,
                type_annotation,
                type_annotation_kind,
                ..
            } => {
                let v = self.resolve_place(value);
                Some(match type_annotation_kind {
                    TypeAnnotationKind::Cast => format!("({}: {})", v, type_annotation),
                    TypeAnnotationKind::As => format!("{} as {}", v, type_annotation),
                    TypeAnnotationKind::Satisfies => {
                        format!("{} satisfies {}", v, type_annotation)
                    }
                })
            }
            _ => None,
        }
    }

    /// Resolve a Place to its expression string.
    fn resolve_place(&self, place: &Place) -> String {
        // Try the expr_map first (for temporaries)
        if let Some(expr) = self.expr_map.get(&place.identifier.id) {
            return expr.clone();
        }
        self.resolve_place_inner(place)
    }

    /// Resolve a Place, wrapping in parens if the resolved expression has lower precedence
    /// than `min_prec`. This prevents incorrect grouping when inlining sub-expressions.
    fn resolve_place_with_min_prec(&self, place: &Place, min_prec: u8) -> String {
        let expr = self.resolve_place(place);
        if let Some(&prec) = self.expr_precedence.get(&place.identifier.id)
            && prec > 0
            && prec < min_prec
        {
            return format!("({})", expr);
        }
        expr
    }

    /// Resolve a Place for use inside a logical expression.
    /// Wraps in parens if the child is a logical/ternary expression with a different
    /// precedence than `parent_prec`. The upstream compiler always adds parens when
    /// mixing different logical operators for clarity.
    fn resolve_place_with_logical_parens(&self, place: &Place, parent_prec: u8) -> String {
        let expr = self.resolve_place(place);
        if let Some(&prec) = self.expr_precedence.get(&place.identifier.id) {
            // Wrap if child is a logical/ternary (prec 3-6) with a different precedence
            if (3..=6).contains(&prec) && prec != parent_prec {
                return format!("({})", expr);
            }
        }
        expr
    }

    fn resolve_place_inner(&self, place: &Place) -> String {
        // Check id-based rename first (for shadowed variables like a → a_0)
        if let Some(renamed) = self.id_rename_map.get(&place.identifier.id) {
            return renamed.clone();
        }
        match &place.identifier.name {
            Some(IdentifierName::Named(name)) => self
                .source_rename_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
            Some(IdentifierName::Promoted(name)) => self
                .source_rename_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
            None => {
                // Use sequential temp names (t0, t1, ...) from the map
                if let Some(name) = self.temp_name_map.get(&place.identifier.id) {
                    name.clone()
                } else {
                    format!("_t{}", place.identifier.id.0)
                }
            }
        }
    }

    /// Resolve a method property Place to either dot name or computed expression.
    /// Returns (property_str, is_computed).
    fn resolve_method_property(&self, place: &Place, receiver_expr: &str) -> (String, bool) {
        let resolved = self.resolve_place(place);
        // Primitive::String("mutate") -> dot notation
        if resolved.starts_with('"') && resolved.ends_with('"') && resolved.len() >= 2 {
            return (resolved[1..resolved.len() - 1].to_string(), false);
        }
        // PropertyLoad lowering can materialize `<receiver>.<prop>` as the property Place.
        // Recover static prop names from that representation.
        if let Some(prop_name) = resolved.strip_prefix(receiver_expr) {
            if let Some(name) = prop_name.strip_prefix('.')
                && !name.is_empty()
                && Self::is_valid_js_identifier_name(name)
            {
                return (name.to_string(), false);
            }
            if let Some(name) = prop_name.strip_prefix("?.")
                && !name.is_empty()
                && Self::is_valid_js_identifier_name(name)
            {
                return (name.to_string(), false);
            }
        }
        (resolved, true)
    }

    fn is_valid_js_identifier_name(name: &str) -> bool {
        if name.is_empty() {
            return false;
        }
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
            return false;
        }
        chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
    }

    fn resolve_args(&self, args: &[Argument]) -> String {
        args.iter()
            .map(|a| match a {
                Argument::Place(p) => self.resolve_place(p),
                Argument::Spread(p) => format!("...{}", self.resolve_place(p)),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Public-facing name resolver for use by collect_scope_codegen_info.
    fn resolve_identifier_name(&self, id: &Identifier) -> String {
        self.identifier_name(id)
    }

    fn identifier_name(&self, id: &Identifier) -> String {
        // Check id-based rename first (for shadowed variables like a → a_0)
        if let Some(renamed) = self.id_rename_map.get(&id.id) {
            return renamed.clone();
        }
        match &id.name {
            Some(IdentifierName::Named(name)) => self
                .source_rename_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
            Some(IdentifierName::Promoted(name)) => self
                .source_rename_map
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.clone()),
            None => {
                if let Some(name) = self.temp_name_map.get(&id.id) {
                    name.clone()
                } else {
                    format!("_t{}", id.id.0)
                }
            }
        }
    }

    fn primitive_str(&self, value: &PrimitiveValue) -> String {
        match value {
            PrimitiveValue::Null => "null".to_string(),
            PrimitiveValue::Undefined => "undefined".to_string(),
            PrimitiveValue::Boolean(b) => b.to_string(),
            PrimitiveValue::Number(n) => format_number(*n),
            PrimitiveValue::String(s) => format!("\"{}\"", escape_js_string(s)),
        }
    }

    fn jsx_to_string(
        &self,
        tag: &JsxTag,
        props: &[JsxAttribute],
        children: Option<&[Place]>,
    ) -> String {
        let tag_str = match tag {
            JsxTag::BuiltinTag(name) => name.clone(),
            JsxTag::Component(place) => self.resolve_place(place),
            JsxTag::Fragment => "".to_string(),
        };

        let mut attrs = String::new();
        for attr in props {
            match attr {
                JsxAttribute::Attribute { name, place } => {
                    let val = self.resolve_place(place);
                    // Use JSXStringLiteral form (no braces) for simple ASCII strings
                    if val.starts_with('"') && val.ends_with('"') && val.len() >= 2 {
                        let inner = &val[1..val.len() - 1];
                        let is_simple = inner.bytes().all(|b| {
                            (0x20..=0x7E).contains(&b) && b != b'\\' && b != b'{' && b != b'}'
                        });
                        if is_simple {
                            attrs.push_str(&format!(" {}={}", name, val));
                        } else {
                            attrs.push_str(&format!(" {}={{{}}}", name, val));
                        }
                    } else {
                        attrs.push_str(&format!(" {}={{{}}}", name, val));
                    }
                }
                JsxAttribute::SpreadAttribute { argument } => {
                    attrs.push_str(&format!(" {{...{}}}", self.resolve_place(argument)));
                }
            }
        }

        match (children, tag.is_fragment()) {
            (None, false) | (Some(&[]), false) => format!("<{}{} />", tag_str, attrs),
            (Some(kids), false) => {
                let joined = self.join_jsx_children(kids);
                format!("<{}{}>{}</{}>", tag_str, attrs, joined, tag_str)
            }
            (None, true) | (Some(&[]), true) => "<></>".to_string(),
            (Some(kids), true) => {
                let joined = self.join_jsx_children(kids);
                format!("<>{}</>", joined)
            }
        }
    }

    /// Render a JSX child — JSXText and JSX elements without braces, expressions with {}.
    fn jsx_child_str(&self, place: &Place) -> String {
        // Check if this child is JSXText (should render without braces).
        // The text was already trimmed by trim_jsx_text during HIR lowering.
        if self.jsx_text_ids.contains(&place.identifier.id) {
            return self.resolve_place(place);
        }
        // JSX element/fragment children render inline, no braces
        if self.jsx_element_ids.contains(&place.identifier.id) {
            return self.resolve_place(place);
        }
        // Expression children get wrapped in {}
        format!("{{{}}}", self.resolve_place(place))
    }

    /// Join JSX children with appropriate spacing.
    /// Babel's generator (compact+retainLines) adds spaces around children when
    /// ALL children are non-text (elements/expressions). When any JSXText child
    /// is present, it provides its own spacing and no extra spaces are added.
    /// Single element children also get spaces; single expression/text children don't.
    fn join_jsx_children(&self, kids: &[Place]) -> String {
        let child_strs: Vec<String> = kids.iter().map(|c| self.jsx_child_str(c)).collect();

        if child_strs.is_empty() {
            return String::new();
        }

        let has_text = kids
            .iter()
            .any(|c| self.jsx_text_ids.contains(&c.identifier.id));
        let has_element = kids
            .iter()
            .any(|c| self.jsx_element_ids.contains(&c.identifier.id));

        // When text is present, it provides spacing — just concatenate
        if has_text {
            return child_strs.join("");
        }

        // Single expression child: no extra spaces
        if child_strs.len() == 1 && !has_element {
            return child_strs.into_iter().next().unwrap();
        }

        // All non-text children: add spaces (Babel's compact+retainLines behavior).
        // This covers: single element, multiple elements, multiple expressions,
        // or mix of elements and expressions.
        let mut result = String::from(" ");
        for (i, s) in child_strs.iter().enumerate() {
            result.push_str(s);
            if i + 1 < child_strs.len() && !result.ends_with(' ') {
                result.push(' ');
            }
        }
        if !result.ends_with(' ') {
            result.push(' ');
        }
        result
    }

    /// Convert a destructuring pattern to its JavaScript string representation.
    fn pattern_to_string(&self, pattern: &Pattern) -> String {
        match pattern {
            Pattern::Array(arr) => {
                let items: Vec<String> = arr
                    .items
                    .iter()
                    .map(|elem| match elem {
                        ArrayElement::Place(place) => self.identifier_name(&place.identifier),
                        ArrayElement::Spread(place) => {
                            format!("...{}", self.identifier_name(&place.identifier))
                        }
                        ArrayElement::Hole => String::new(),
                    })
                    .collect();
                format!("[{}]", items.join(", "))
            }
            Pattern::Object(obj) => {
                let props: Vec<String> = obj
                    .properties
                    .iter()
                    .map(|prop| {
                        match prop {
                            ObjectPropertyOrSpread::Property(p) => {
                                let key = match &p.key {
                                    ObjectPropertyKey::String(s) => {
                                        // If the key contains non-identifier chars, quote it
                                        if s.chars()
                                            .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                                        {
                                            s.clone()
                                        } else {
                                            format!("\"{}\"", s)
                                        }
                                    }
                                    ObjectPropertyKey::Identifier(s) => s.clone(),
                                    ObjectPropertyKey::Number(n) => format_number(*n),
                                    ObjectPropertyKey::Computed(p) => {
                                        format!("[{}]", self.resolve_place(p))
                                    }
                                };
                                let binding = self.identifier_name(&p.place.identifier);
                                if key == binding {
                                    key // shorthand: { x }
                                } else {
                                    format!("{}: {}", key, binding) // { key: binding }
                                }
                            }
                            ObjectPropertyOrSpread::Spread(place) => {
                                format!("...{}", self.resolve_place(place))
                            }
                        }
                    })
                    .collect();
                format!("{{ {} }}", props.join(", "))
            }
        }
    }

    /// Generate the body code for an inner function (lambda, function expression).
    /// This uses a simplified approach — it doesn't create a full memo scope,
    /// just emits the statements and return.
    fn generate_inner_function_body(&self, func: &HIRFunction) -> String {
        let mut inner_consumed: HashSet<IdentifierId> = HashSet::new();

        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                    if place.identifier.name.is_none() {
                        inner_consumed.insert(place.identifier.id);
                    }
                });
            }
            match &block.terminal {
                Terminal::Return { value, .. } | Terminal::Throw { value, .. } => {
                    if value.identifier.name.is_none() {
                        inner_consumed.insert(value.identifier.id);
                    }
                }
                _ => {}
            }
        }

        // Create a temporary inner codegen context that shares the inner expr_map
        let mut inner_cg = CodeGenerator::new();
        inner_cg.consumed_temps = inner_consumed;
        inner_cg.indent = 0;

        // Detect shadowed variables: inner NEW declarations (let/const, not reassignments)
        // that reuse names from the outer function scope. Rename the inner ones with _N suffix.
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                let decl_name_id = match &instr.value {
                    InstructionValue::StoreLocal { lvalue, .. } => {
                        // Only new declarations, not reassignments of captured outer variables
                        if lvalue.kind != InstructionKind::Reassign {
                            match &lvalue.place.identifier.name {
                                Some(IdentifierName::Named(n))
                                | Some(IdentifierName::Promoted(n)) => {
                                    Some((n.clone(), lvalue.place.identifier.id))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    InstructionValue::DeclareLocal { lvalue, .. } => {
                        match &lvalue.place.identifier.name {
                            Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => {
                                Some((n.clone(), lvalue.place.identifier.id))
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                if let Some((name, id)) = decl_name_id
                    && self.outer_scope_names.contains(&name)
                {
                    // This inner declaration shadows an outer variable
                    let mut suffix = 0u32;
                    let mut new_name = format!("{}_{}", name, suffix);
                    while self.outer_scope_names.contains(&new_name) {
                        suffix += 1;
                        new_name = format!("{}_{}", name, suffix);
                    }
                    inner_cg.id_rename_map.insert(id, new_name);
                }
            }
        }

        // Assign sequential temp names (t0, t1, ...) for unnamed params
        for param in &func.params {
            let id = match param {
                Argument::Place(p) => p.identifier.id,
                Argument::Spread(p) => p.identifier.id,
            };
            let has_name = match param {
                Argument::Place(p) => p.identifier.name.is_some(),
                Argument::Spread(p) => p.identifier.name.is_some(),
            };
            if !has_name {
                inner_cg.assign_temp_name(id);
            }
        }

        // Build inner expr_map
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                if matches!(&instr.value, InstructionValue::JSXText { .. }) {
                    inner_cg.jsx_text_ids.insert(instr.lvalue.identifier.id);
                }
                if matches!(
                    &instr.value,
                    InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }
                ) {
                    inner_cg.jsx_element_ids.insert(instr.lvalue.identifier.id);
                }
                if let InstructionValue::StoreLocal { lvalue, .. } = &instr.value
                    && lvalue.kind == InstructionKind::Reassign
                    && let Some(name) = &lvalue.place.identifier.name
                {
                    match name {
                        IdentifierName::Named(n) | IdentifierName::Promoted(n) => {
                            inner_cg.reassigned_vars.insert(n.clone());
                        }
                    }
                }
                let expr = inner_cg.instruction_to_expr(instr);
                if let Some(expr_str) = expr {
                    inner_cg
                        .expr_map
                        .insert(instr.lvalue.identifier.id, expr_str);
                }
            }
        }

        // Assign sequential temp names for unnamed non-inlined instructions
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                let is_pattern_instr = matches!(
                    &instr.value,
                    InstructionValue::Destructure { .. }
                        | InstructionValue::DeclareLocal { .. }
                        | InstructionValue::DeclareContext { .. }
                );
                if instr.lvalue.identifier.name.is_none()
                    && !inner_cg.expr_map.contains_key(&instr.lvalue.identifier.id)
                    && !is_pattern_instr
                {
                    inner_cg.assign_temp_name(instr.lvalue.identifier.id);
                }
            }
        }

        // Rename catch clause bindings to temp names.
        // For outlined functions (not in catch_rename_map), use the inner counter.
        // For inline functions, the outer analyze() will post-process the expr_map
        // after catch_rename_map is populated.
        for (_, block) in &func.body.blocks {
            if let Terminal::Try {
                handler_binding: Some(binding),
                ..
            } = &block.terminal
                && binding.identifier.name.is_some()
            {
                let temp_name = format!("t{}", inner_cg.temp_name_counter);
                inner_cg.temp_name_counter += 1;
                inner_cg
                    .id_rename_map
                    .insert(binding.identifier.id, temp_name);
            }
        }

        // Fix up inner expr_map: replace _t{id} references with assigned tN names
        if !inner_cg.temp_name_map.is_empty() {
            let replacements: Vec<(String, String)> = inner_cg
                .temp_name_map
                .iter()
                .map(|(id, name)| (format!("_t{}", id.0), name.clone()))
                .collect();
            for expr in inner_cg.expr_map.values_mut() {
                for (old, new) in &replacements {
                    if expr.contains(old.as_str()) {
                        *expr = expr.replace(old.as_str(), new.as_str());
                    }
                }
            }
        }

        // Build block map for structural terminal handling
        let block_map: HashMap<BlockId, &BasicBlock> = func
            .body
            .blocks
            .iter()
            .map(|(id, block)| (*id, block))
            .collect();

        // Build owned_blocks set (blocks that are part of a terminal's structure)
        let mut owned_blocks: HashSet<BlockId> = HashSet::new();
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    if let Some(u) = update {
                        owned_blocks.insert(*u);
                    }
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForOf {
                    init,
                    test,
                    loop_block,
                    ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    owned_blocks.insert(*init);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    owned_blocks.insert(*test);
                    owned_blocks.insert(*loop_block);
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    owned_blocks.insert(*loop_block);
                    owned_blocks.insert(*test);
                }
                Terminal::If {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    owned_blocks.insert(*consequent);
                    if *alternate != *fallthrough {
                        owned_blocks.insert(*alternate);
                    }
                }
                Terminal::Try {
                    block: try_block,
                    handler,
                    ..
                } => {
                    owned_blocks.insert(*try_block);
                    owned_blocks.insert(*handler);
                }
                Terminal::Switch { cases, .. } => {
                    for case in cases {
                        owned_blocks.insert(case.block);
                    }
                }
                _ => {}
            }
        }

        // Transitively own blocks reachable within loop/if bodies
        for (_, block) in &func.body.blocks {
            match &block.terminal {
                Terminal::For {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForOf {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::ForIn {
                    init,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *init,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::While {
                    test,
                    loop_block,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                Terminal::DoWhile {
                    loop_block,
                    test,
                    fallthrough,
                    ..
                } => {
                    collect_loop_body_owned_blocks(
                        *loop_block,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                    collect_loop_body_owned_blocks(
                        *test,
                        *fallthrough,
                        &block_map,
                        &mut owned_blocks,
                    );
                }
                _ => {}
            }
        }

        // Collect statements and return
        let mut stmts: Vec<String> = Vec::new();
        let mut return_expr: Option<String> = None;

        for (_, block) in &func.body.blocks {
            if owned_blocks.contains(&block.id) {
                continue;
            }

            for instr in &block.instructions {
                if let Some(stmt) = inner_cg.instruction_to_stmt(instr) {
                    stmts.push(stmt);
                }
            }

            // Handle structural terminals
            match &block.terminal {
                Terminal::Return {
                    value,
                    return_variant,
                    ..
                } => {
                    if *return_variant == ReturnVariant::Explicit
                        || *return_variant == ReturnVariant::Implicit
                    {
                        return_expr = Some(inner_cg.resolve_place(value));
                    }
                }
                Terminal::Throw { value, .. } => {
                    let val = inner_cg.resolve_place(value);
                    stmts.push(format!("throw {};", val));
                }
                Terminal::If {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                }
                | Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough,
                    ..
                } => {
                    let if_str = inner_cg.emit_if_terminal(
                        test,
                        *consequent,
                        *alternate,
                        *fallthrough,
                        &block_map,
                        &owned_blocks,
                    );
                    if let Some(s) = if_str {
                        stmts.push(s);
                    }
                }
                Terminal::Try {
                    block: try_block,
                    handler_binding,
                    handler,
                    ..
                } => {
                    let try_stmts =
                        inner_cg.collect_loop_body_stmts(*try_block, &block_map, &owned_blocks);
                    // Empty try block elimination
                    if !try_stmts.is_empty() {
                        let catch_stmts =
                            inner_cg.collect_loop_body_stmts(*handler, &block_map, &owned_blocks);
                        let catch_param = handler_binding
                            .as_ref()
                            .map(|p| inner_cg.resolve_place(p))
                            .unwrap_or_default();
                        let mut try_str = "try {".to_string();
                        for s in &try_stmts {
                            try_str.push_str(&format!("\n{}", s));
                        }
                        if catch_param.is_empty() {
                            try_str.push_str("\n} catch {");
                        } else {
                            try_str.push_str(&format!("\n}} catch ({}) {{", catch_param));
                        }
                        for s in &catch_stmts {
                            try_str.push_str(&format!("\n{}", s));
                        }
                        try_str.push_str("\n}");
                        stmts.push(try_str);
                    }
                }
                Terminal::For {
                    init,
                    test,
                    update,
                    loop_block,
                    ..
                } => {
                    let init_s = inner_cg.get_for_init_str(*init, &block_map);
                    let test_expr = inner_cg.get_block_expr_with_assignments(*test, &block_map);
                    let update_expr = update
                        .map(|u| inner_cg.get_for_update_expr(u, &block_map))
                        .unwrap_or_default();
                    let body_stmts =
                        inner_cg.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str =
                        format!("for ({}; {}; {}) {{", init_s, test_expr, update_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    stmts.push(loop_str);
                }
                Terminal::ForOf {
                    init, loop_block, ..
                } => {
                    let init_expr = inner_cg.get_block_expr(*init, &block_map);
                    let body_stmts =
                        inner_cg.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        inner_cg.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} of {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    stmts.push(loop_str);
                }
                Terminal::ForIn {
                    init, loop_block, ..
                } => {
                    let init_expr = inner_cg.get_block_expr(*init, &block_map);
                    let body_stmts =
                        inner_cg.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let (var_decl, skip_count) =
                        inner_cg.get_for_in_of_var_decl(*loop_block, &block_map);
                    let rest_body: Vec<String> =
                        if !var_decl.is_empty() && body_stmts.len() > skip_count {
                            body_stmts[skip_count..].to_vec()
                        } else {
                            body_stmts.clone()
                        };
                    let mut loop_str = format!("for ({} in {}) {{", var_decl, init_expr);
                    for s in &rest_body {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    stmts.push(loop_str);
                }
                Terminal::While {
                    test, loop_block, ..
                } => {
                    let test_expr = inner_cg.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        inner_cg.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = format!("while ({}) {{", test_expr);
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str("\n}");
                    stmts.push(loop_str);
                }
                Terminal::DoWhile {
                    loop_block, test, ..
                } => {
                    let test_expr = inner_cg.get_block_expr_with_assignments(*test, &block_map);
                    let body_stmts =
                        inner_cg.collect_loop_body_stmts(*loop_block, &block_map, &owned_blocks);
                    let mut loop_str = "do {".to_string();
                    for s in &body_stmts {
                        loop_str.push_str(&format!("\n  {}", s));
                    }
                    loop_str.push_str(&format!("\n}} while ({});", test_expr));
                    stmts.push(loop_str);
                }
                Terminal::Switch { test, cases, .. } => {
                    let test_expr = inner_cg.resolve_place(test);
                    let mut switch_str = format!("switch ({}) {{", test_expr);
                    for case in cases {
                        let case_stmts =
                            inner_cg.collect_loop_body_stmts(case.block, &block_map, &owned_blocks);
                        if let Some(test_place) = &case.test {
                            let case_test = inner_cg.resolve_place(test_place);
                            switch_str.push_str(&format!("\ncase {}:", case_test));
                        } else {
                            switch_str.push_str("\ndefault:");
                        }
                        for s in &case_stmts {
                            switch_str.push_str(&format!("\n  {}", s));
                        }
                    }
                    switch_str.push_str("\n}");
                    stmts.push(switch_str);
                }
                _ => {}
            }
        }

        // Emit directives at the start of the body
        let mut body_lines: Vec<String> = Vec::new();
        for directive in &func.directives {
            body_lines.push(format!("\"{}\";", directive));
        }
        body_lines.extend(stmts);
        if let Some(ret) = return_expr
            && ret != "undefined"
        {
            body_lines.push(format!("return {};", ret));
        }

        // Indent body lines by one level (relative to the opening brace)
        body_lines
            .iter()
            .map(|line| format!("  {}", line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Generate parameter names for an inner function.
    fn generate_inner_function_params(&self, func: &HIRFunction) -> String {
        let mut temp_counter = 0usize;
        func.params
            .iter()
            .map(|param| match param {
                Argument::Place(place) => match &place.identifier.name {
                    Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
                        name.clone()
                    }
                    None => {
                        let name = format!("t{}", temp_counter);
                        temp_counter += 1;
                        name
                    }
                },
                Argument::Spread(place) => match &place.identifier.name {
                    Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
                        format!("...{}", name)
                    }
                    None => {
                        let name = format!("...t{}", temp_counter);
                        temp_counter += 1;
                        name
                    }
                },
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn emit_line(&mut self, line: &str) {
        let indent = "  ".repeat(self.indent);
        // Handle multi-line strings (e.g., inner function bodies)
        // — indent every line, not just the first
        if line.contains('\n') {
            for (i, sub_line) in line.split('\n').enumerate() {
                if i > 0 {
                    self.output.push('\n');
                }
                self.output.push_str(&indent);
                self.output.push_str(sub_line);
            }
            self.output.push('\n');
        } else {
            self.output.push_str(&indent);
            self.output.push_str(line);
            self.output.push('\n');
        }
    }
}

impl JsxTag {
    fn is_fragment(&self) -> bool {
        matches!(self, JsxTag::Fragment)
    }
}

fn binary_op_str(op: &BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Eq => "==",
        BinaryOperator::NotEq => "!=",
        BinaryOperator::StrictEq => "===",
        BinaryOperator::StrictNotEq => "!==",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::LShift => "<<",
        BinaryOperator::RShift => ">>",
        BinaryOperator::URShift => ">>>",
        BinaryOperator::Add => "+",
        BinaryOperator::Sub => "-",
        BinaryOperator::Mul => "*",
        BinaryOperator::Div => "/",
        BinaryOperator::Mod => "%",
        BinaryOperator::Exp => "**",
        BinaryOperator::BitOr => "|",
        BinaryOperator::BitXor => "^",
        BinaryOperator::BitAnd => "&",
        BinaryOperator::In => "in",
        BinaryOperator::InstanceOf => "instanceof",
    }
}

fn unary_op_str(op: &UnaryOperator) -> &'static str {
    match op {
        UnaryOperator::Minus => "-",
        UnaryOperator::Plus => "+",
        UnaryOperator::Not => "!",
        UnaryOperator::BitNot => "~",
        UnaryOperator::TypeOf => "typeof ",
        UnaryOperator::Void => "void ",
    }
}

/// Return the JS operator precedence level for an instruction's output expression.
/// Higher number = higher precedence (tighter binding).
/// Returns 0 for non-operator expressions (no parens needed).
fn instr_precedence(value: &InstructionValue) -> u8 {
    match value {
        InstructionValue::Ternary { .. } => 3,
        InstructionValue::LogicalExpression { operator, .. } => logical_op_precedence(operator),
        InstructionValue::BinaryExpression { operator, .. } => binary_op_precedence(operator),
        _ => 0,
    }
}

fn logical_op_precedence(op: &LogicalOperator) -> u8 {
    match op {
        LogicalOperator::NullishCoalescing => 4,
        LogicalOperator::Or => 5,
        LogicalOperator::And => 6,
    }
}

fn binary_op_precedence(op: &BinaryOperator) -> u8 {
    match op {
        BinaryOperator::BitOr => 7,
        BinaryOperator::BitXor => 8,
        BinaryOperator::BitAnd => 9,
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::StrictEq
        | BinaryOperator::StrictNotEq => 10,
        BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::In
        | BinaryOperator::InstanceOf => 11,
        BinaryOperator::LShift | BinaryOperator::RShift | BinaryOperator::URShift => 12,
        BinaryOperator::Add | BinaryOperator::Sub => 13,
        BinaryOperator::Mul | BinaryOperator::Div | BinaryOperator::Mod => 14,
        BinaryOperator::Exp => 15,
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.is_finite() && (0.0..1e15).contains(&n) {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Check if a string is a valid JavaScript identifier (for dot notation).
fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Replace whole-word occurrences of `old` with `new_val` in a string.
/// A word boundary is a position where the adjacent character is not alphanumeric, '_', or '$'.
fn replace_whole_word(s: &str, old: &str, new_val: &str) -> String {
    if old.is_empty() || !s.contains(old) {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let old_bytes = old.as_bytes();
    let old_len = old_bytes.len();
    let mut i = 0;
    while i <= bytes.len() - old_len {
        if &bytes[i..i + old_len] == old_bytes {
            // Check word boundary before
            let before_ok = i == 0 || !is_word_char(bytes[i - 1]);
            // Check word boundary after
            let after_ok = i + old_len >= bytes.len() || !is_word_char(bytes[i + old_len]);
            if before_ok && after_ok {
                result.push_str(new_val);
                i += old_len;
                continue;
            }
        }
        result.push(bytes[i] as char);
        i += 1;
    }
    // Append remaining bytes
    while i < bytes.len() {
        result.push(bytes[i] as char);
        i += 1;
    }
    result
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Escape a string for JavaScript output (double-quoted context).
fn escape_js_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{0008}' => result.push_str("\\b"),
            '\u{000C}' => result.push_str("\\f"),
            '\u{000B}' => result.push_str("\\v"),
            '\0' => result.push_str("\\0"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04X}", c as u32));
            }
            // Escape non-ASCII characters to \uXXXX (matching Babel's jsesc behavior)
            c if !c.is_ascii() => {
                // For characters in BMP (Basic Multilingual Plane)
                let code = c as u32;
                if code <= 0xFFFF {
                    result.push_str(&format!("\\u{:04X}", code));
                } else {
                    // Surrogate pair for supplementary characters
                    let code = code - 0x10000;
                    let high = 0xD800 + (code >> 10);
                    let low = 0xDC00 + (code & 0x3FF);
                    result.push_str(&format!("\\u{:04X}\\u{:04X}", high, low));
                }
            }
            _ => result.push(c),
        }
    }
    result
}

/// Reorder statements so that `let <name>;` declarations come before any `<name> = ...;`
/// assignments. This fixes ordering when phi elimination + block simplification produces
/// reassignments in earlier blocks than declarations.
fn reorder_declarations(stmts: &mut Vec<String>) {
    use std::collections::HashMap;

    // Find `let <name>;` declarations (no initializer)
    let mut decl_positions: HashMap<String, usize> = HashMap::new();
    for (i, stmt) in stmts.iter().enumerate() {
        let trimmed = stmt.trim();
        if trimmed.starts_with("let ") && trimmed.ends_with(';') && !trimmed.contains('=') {
            // Extract variable name: "let x;" → "x"
            let name = trimmed[4..trimmed.len() - 1].trim().to_string();
            if is_valid_identifier(&name) {
                decl_positions.insert(name, i);
            }
        }
    }

    if decl_positions.is_empty() {
        return;
    }

    // Find the earliest assignment to each declared variable
    for (name, decl_pos) in &decl_positions {
        let assign_prefix = format!("{} = ", name);
        let mut earliest_assign = None;
        for (i, stmt) in stmts.iter().enumerate() {
            if i == *decl_pos {
                continue;
            }
            let trimmed = stmt.trim();
            if trimmed.starts_with(&assign_prefix) {
                earliest_assign = Some(i);
                break;
            }
        }

        if let Some(assign_pos) = earliest_assign
            && assign_pos < *decl_pos
        {
            // Declaration is after assignment — move declaration before assignment
            let decl_stmt = stmts.remove(*decl_pos);
            // After removal, assign_pos may have shifted if it was after decl_pos
            // But we know assign_pos < decl_pos, so it's not affected
            stmts.insert(assign_pos, decl_stmt);
        }
    }
}

/// Check if a return expression string is a literal primitive value.
/// Literal primitives don't benefit from memoization since React's Object.is()
/// handles them by value.
fn is_literal_primitive(expr: &str) -> bool {
    let s = expr.trim();
    if s.is_empty() {
        return false;
    }
    // null, undefined, void 0
    if s == "null" || s == "undefined" || s == "void 0" {
        return true;
    }
    // boolean
    if s == "true" || s == "false" {
        return true;
    }
    // number (integer or float, possibly negative)
    if s.parse::<f64>().is_ok() {
        return true;
    }
    // Negative numbers with unary minus that parse::<f64> handles
    if let Some(rest) = s.strip_prefix('-')
        && rest.parse::<f64>().is_ok()
    {
        return true;
    }
    // string literal
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        return true;
    }
    false
}
