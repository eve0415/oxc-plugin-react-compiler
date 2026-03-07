//! Prune non-escaping reactive scopes.
//!
//! Port of `PruneNonEscapingScopes.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This pass prunes reactive scopes that are not necessary to bound downstream
//! computation. Specifically, the pass identifies the set of identifiers which
//! may "escape". Values can escape in one of two ways:
//!
//! * They are directly returned by the function and/or transitively aliased by
//!   a return value.
//! * They are passed as input to a hook. Any value passed to a hook may have
//!   its reference ultimately stored by React (i.e., be aliased by an external
//!   value). For example, the closure passed to useEffect escapes.
//!
//! ## Algorithm
//!
//! 1. Build a graph mapping DeclarationId to a node describing all the scopes
//!    and inputs involved in creating that identifier. Individual nodes are
//!    marked as definitely aliased, conditionally aliased, or unaliased.
//! 2. The same traversal stores the set of returned identifiers and identifiers
//!    passed as arguments to hooks.
//! 3. Walk the graph from the returned identifiers and mark reachable
//!    dependencies as escaping.
//! 4. Prune scopes whose outputs were not marked.

use std::collections::{HashMap, HashSet};

use crate::environment::Environment;
use crate::hir::types::*;
use crate::inference::infer_mutation_aliasing_effects::get_function_call_signature;

fn debug_scope_prune(scope: &ReactiveScope, pass: &str, reason: &str) {
    if std::env::var("DEBUG_SCOPE_PRUNE_REASON").is_ok() {
        let deps = scope
            .dependencies
            .iter()
            .map(|dep| dep.identifier.declaration_id.0)
            .collect::<Vec<_>>();
        let decls = scope
            .declarations
            .values()
            .map(|decl| decl.identifier.declaration_id.0)
            .collect::<Vec<_>>();
        eprintln!(
            "[SCOPE_PRUNE_REASON] scope={} pass={} reason={} range=({}, {}) deps={:?} decls={:?}",
            scope.id.0, pass, reason, scope.range.start.0, scope.range.end.0, deps, decls
        );
    }
}

// ---------------------------------------------------------------------------
// MemoizationLevel
// ---------------------------------------------------------------------------

/// Describes how to determine whether a value should be memoized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoizationLevel {
    /// The value should be memoized if it escapes.
    Memoized,
    /// The value is memoized only if its dependencies are memoized (used for
    /// logical/ternary and other forwarding expressions).
    Conditional,
    /// Values that cannot be compared with `Object.is`, but which by default
    /// do not need to be memoized unless forced.
    Unmemoized,
    /// The value will never be memoized (cheaply compared with `Object.is`).
    Never,
}

/// Given an identifier that appears as an lvalue multiple times with different
/// memoization levels, determines the final memoization level.
fn join_aliases(kind1: MemoizationLevel, kind2: MemoizationLevel) -> MemoizationLevel {
    use MemoizationLevel::*;
    if kind1 == Memoized || kind2 == Memoized {
        Memoized
    } else if kind1 == Conditional || kind2 == Conditional {
        Conditional
    } else if kind1 == Unmemoized || kind2 == Unmemoized {
        Unmemoized
    } else {
        Never
    }
}

// ---------------------------------------------------------------------------
// Graph nodes
// ---------------------------------------------------------------------------

/// A node describing the memoization level of a given identifier as well as
/// its dependencies and scopes.
struct IdentifierNode {
    level: MemoizationLevel,
    memoized: bool,
    dependencies: HashSet<DeclarationId>,
    scopes: HashSet<ScopeId>,
    seen: bool,
}

/// A scope node describing its dependencies.
struct ScopeNode {
    dependencies: Vec<DeclarationId>,
    seen: bool,
}

// ---------------------------------------------------------------------------
// Memoization options
// ---------------------------------------------------------------------------

struct MemoizationOptions {
    memoize_jsx_elements: bool,
    force_memoize_primitives: bool,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Stores the identifier and scope graphs, set of escaping identifiers, etc.
struct State {
    /// Maps lvalues for LoadLocal to the identifier being loaded, to resolve
    /// indirections in subsequent lvalues/rvalues.
    ///
    /// Uses DeclarationId rather than IdentifierId because the pass is not
    /// aware of control-flow, only data flow via mutation.
    ///
    /// Upstream only needs a single source per declaration because
    /// BuildReactiveFunction preserves nested value structure. This port
    /// flattens value blocks, which can produce multiple `LoadLocal`s that all
    /// feed the same merged declaration (for example ternary branches lowered
    /// to the same temporary). Track all possible sources so the escape graph
    /// can still recover the aliased values.
    definitions: HashMap<DeclarationId, HashSet<DeclarationId>>,
    identifiers: HashMap<DeclarationId, IdentifierNode>,
    scopes: HashMap<ScopeId, ScopeNode>,
    escaping_values: HashSet<DeclarationId>,
}

impl State {
    fn new() -> Self {
        Self {
            definitions: HashMap::new(),
            identifiers: HashMap::new(),
            scopes: HashMap::new(),
            escaping_values: HashSet::new(),
        }
    }

    /// Declare a new identifier, used for function params.
    fn declare(&mut self, id: DeclarationId) {
        self.identifiers.insert(
            id,
            IdentifierNode {
                level: MemoizationLevel::Never,
                memoized: false,
                dependencies: HashSet::new(),
                scopes: HashSet::new(),
                seen: false,
            },
        );
    }

    /// Resolve an identifier through all transitive LoadLocal definitions.
    fn resolve_all(&self, id: DeclarationId) -> HashSet<DeclarationId> {
        let mut resolved = HashSet::new();
        let mut stack = vec![id];
        let mut seen = HashSet::new();

        while let Some(current) = stack.pop() {
            if !seen.insert(current) {
                continue;
            }
            match self.definitions.get(&current) {
                Some(nexts) if !nexts.is_empty() => {
                    let mut pushed = false;
                    for &next in nexts {
                        if next != current {
                            stack.push(next);
                            pushed = true;
                        }
                    }
                    if !pushed {
                        resolved.insert(current);
                    }
                }
                _ => {
                    resolved.insert(current);
                }
            }
        }

        if resolved.is_empty() {
            resolved.insert(id);
        }
        resolved
    }

    /// Collapse transitive LoadLocal definitions when they resolve to a single
    /// unambiguous declaration. Otherwise keep the original merged declaration.
    fn resolve_unique(&self, id: DeclarationId) -> DeclarationId {
        let resolved = self.resolve_all(id);
        if resolved.len() == 1 {
            resolved.into_iter().next().unwrap()
        } else {
            id
        }
    }

    fn insert_definition(&mut self, target: DeclarationId, source: DeclarationId) {
        self.definitions.entry(target).or_default().insert(source);
    }

    /// Associates the identifier with its scope, if there is one and it is
    /// active for the given instruction id.
    fn visit_operand(&mut self, instr_id: InstructionId, place: &Place, identifier: DeclarationId) {
        if let Some(scope) = get_place_scope(instr_id, place) {
            let scope_id = scope.id;
            self.scopes.entry(scope_id).or_insert_with(|| ScopeNode {
                dependencies: scope
                    .dependencies
                    .iter()
                    .map(|dep| dep.identifier.declaration_id)
                    .collect(),
                seen: false,
            });
            if let Some(identifier_node) = self.identifiers.get_mut(&identifier) {
                identifier_node.scopes.insert(scope_id);
            }
        }
    }

    /// Ensure an identifier node exists, creating it if needed.
    fn ensure_identifier(&mut self, id: DeclarationId) -> &mut IdentifierNode {
        self.identifiers
            .entry(id)
            .or_insert_with(|| IdentifierNode {
                level: MemoizationLevel::Never,
                memoized: false,
                dependencies: HashSet::new(),
                scopes: HashSet::new(),
                seen: false,
            })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the reactive scope for a place if the scope is active at the
/// given instruction id.
fn get_place_scope(id: InstructionId, place: &Place) -> Option<&ReactiveScope> {
    if let Some(scope) = &place.identifier.scope
        && id.0 >= scope.range.start.0
        && id.0 < scope.range.end.0
    {
        return Some(scope.as_ref());
    }
    None
}

/// Returns true if the effect represents a mutable operation.
fn is_mutable_effect(effect: Effect) -> bool {
    matches!(
        effect,
        Effect::Capture
            | Effect::Store
            | Effect::ConditionallyMutate
            | Effect::ConditionallyMutateIterator
            | Effect::Mutate
    )
}

fn is_mutable_operand(place: &Place) -> bool {
    is_mutable_effect(place.effect)
}

fn instruction_value_kind(value: &InstructionValue) -> &'static str {
    match value {
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::TaggedTemplateExpression { .. } => "TaggedTemplateExpression",
        InstructionValue::FunctionExpression { .. } => "FunctionExpression",
        InstructionValue::ObjectMethod { .. } => "ObjectMethod",
        InstructionValue::ArrayExpression { .. } => "ArrayExpression",
        InstructionValue::ObjectExpression { .. } => "ObjectExpression",
        InstructionValue::NewExpression { .. } => "NewExpression",
        InstructionValue::PropertyStore { .. } => "PropertyStore",
        _ => "Other",
    }
}

/// Check if an identifier is a hook based on its name.
fn is_hook_identifier(ident: &Identifier) -> bool {
    match &ident.name {
        Some(name) => Environment::is_hook_name(name.value()),
        None => false,
    }
}

/// Build identifier-name and load-source lookup maps for hook callee detection.
///
/// Lowering often turns a call like `useEffect(cb)` into:
/// `t1 = LoadGlobal(useEffect)` then `CallExpression(callee=t1, args=[...])`.
/// The call-site callee may have no direct name, so we trace aliases through
/// LoadLocal/LoadContext/TypeCastExpression to recover hook identity.
fn build_name_and_load_lookups(
    func: &ReactiveFunction,
) -> (
    HashMap<IdentifierId, String>,
    HashMap<IdentifierId, IdentifierId>,
) {
    fn walk_block(
        block: &ReactiveBlock,
        id_to_name: &mut HashMap<IdentifierId, String>,
        load_source: &mut HashMap<IdentifierId, IdentifierId>,
    ) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    let Some(lvalue) = instr.lvalue.as_ref() else {
                        continue;
                    };

                    if let Some(name) = &lvalue.identifier.name {
                        id_to_name.insert(lvalue.identifier.id, name.value().to_string());
                    }

                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } => {
                            load_source.insert(lvalue.identifier.id, place.identifier.id);
                            if let Some(name) = &place.identifier.name {
                                id_to_name.insert(lvalue.identifier.id, name.value().to_string());
                            }
                            if !id_to_name.contains_key(&lvalue.identifier.id)
                                && let Some(name) = id_to_name.get(&place.identifier.id)
                            {
                                id_to_name.insert(lvalue.identifier.id, name.clone());
                            }
                        }
                        InstructionValue::TypeCastExpression { value, .. } => {
                            load_source.insert(lvalue.identifier.id, value.identifier.id);
                            if !id_to_name.contains_key(&lvalue.identifier.id)
                                && let Some(name) = id_to_name.get(&value.identifier.id)
                            {
                                id_to_name.insert(lvalue.identifier.id, name.clone());
                            }
                        }
                        InstructionValue::LoadGlobal { binding, .. } => {
                            id_to_name.insert(
                                lvalue.identifier.id,
                                load_global_name_for_hook_detection(binding),
                            );
                        }
                        InstructionValue::PropertyLoad { property, .. } => {
                            if let PropertyLiteral::String(name) = property {
                                // Lowered namespace hook calls often flow through an
                                // unnamed PropertyLoad temp (e.g. React.useEffect).
                                // Preserve the property literal so hook detection can
                                // recover the hook identity from the temp id.
                                id_to_name.insert(lvalue.identifier.id, name.clone());
                            }
                        }
                        InstructionValue::Primitive { value, .. } => {
                            if let PrimitiveValue::String(name) = value {
                                id_to_name.insert(lvalue.identifier.id, name.clone());
                            }
                        }
                        _ => {}
                    }
                }
                ReactiveStatement::Scope(scope_block) => {
                    walk_block(&scope_block.instructions, id_to_name, load_source);
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    walk_block(&scope_block.instructions, id_to_name, load_source);
                }
                ReactiveStatement::Terminal(term_stmt) => match &term_stmt.terminal {
                    ReactiveTerminal::If {
                        consequent,
                        alternate,
                        ..
                    } => {
                        walk_block(consequent, id_to_name, load_source);
                        if let Some(alt) = alternate {
                            walk_block(alt, id_to_name, load_source);
                        }
                    }
                    ReactiveTerminal::Switch { cases, .. } => {
                        for case in cases {
                            if let Some(block) = &case.block {
                                walk_block(block, id_to_name, load_source);
                            }
                        }
                    }
                    ReactiveTerminal::DoWhile { loop_block, .. }
                    | ReactiveTerminal::While { loop_block, .. } => {
                        walk_block(loop_block, id_to_name, load_source);
                    }
                    ReactiveTerminal::For {
                        init,
                        update,
                        loop_block,
                        ..
                    } => {
                        walk_block(init, id_to_name, load_source);
                        if let Some(upd) = update {
                            walk_block(upd, id_to_name, load_source);
                        }
                        walk_block(loop_block, id_to_name, load_source);
                    }
                    ReactiveTerminal::ForOf {
                        init, loop_block, ..
                    }
                    | ReactiveTerminal::ForIn {
                        init, loop_block, ..
                    } => {
                        walk_block(init, id_to_name, load_source);
                        walk_block(loop_block, id_to_name, load_source);
                    }
                    ReactiveTerminal::Label { block, .. } => {
                        walk_block(block, id_to_name, load_source);
                    }
                    ReactiveTerminal::Try { block, handler, .. } => {
                        walk_block(block, id_to_name, load_source);
                        walk_block(handler, id_to_name, load_source);
                    }
                    ReactiveTerminal::Break { .. }
                    | ReactiveTerminal::Continue { .. }
                    | ReactiveTerminal::Return { .. }
                    | ReactiveTerminal::Throw { .. } => {}
                },
            }
        }
    }

    let mut id_to_name = HashMap::new();
    let mut load_source = HashMap::new();
    walk_block(&func.body, &mut id_to_name, &mut load_source);
    (id_to_name, load_source)
}

fn is_hook_callee(
    ident: &Identifier,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) -> bool {
    if is_hook_function_type(&ident.type_) {
        return true;
    }

    if is_hook_identifier(ident) {
        return true;
    }

    if let Some(name) = id_to_name.get(&ident.id)
        && Environment::is_hook_name(name)
    {
        return true;
    }

    let mut current = ident.id;
    let mut visited: HashSet<IdentifierId> = HashSet::new();
    while let Some(next) = load_source.get(&current).copied() {
        if !visited.insert(next) {
            break;
        }
        if let Some(name) = id_to_name.get(&next)
            && Environment::is_hook_name(name)
        {
            return true;
        }
        current = next;
    }

    false
}

fn resolve_identifier_name(
    ident: &Identifier,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) -> Option<String> {
    if let Some(name) = ident.name.as_ref().map(IdentifierName::value) {
        return Some(name.to_string());
    }

    if let Some(name) = id_to_name.get(&ident.id) {
        return Some(name.clone());
    }

    let mut current = ident.id;
    let mut visited: HashSet<IdentifierId> = HashSet::new();
    while let Some(next) = load_source.get(&current).copied() {
        if !visited.insert(next) {
            break;
        }
        if let Some(name) = id_to_name.get(&next) {
            return Some(name.clone());
        }
        current = next;
    }

    None
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
                | "BuiltInUseTransitionHookId"
                | "BuiltInUseImperativeHandleHookId"
                | "BuiltInUseActionStateHookId"
                | "BuiltInDefaultMutatingHookId"
                | "BuiltInDefaultNonmutatingHookId"
        ),
        _ => false,
    }
}

fn has_no_alias_function_signature(ty: &Type) -> bool {
    get_function_call_signature(ty).is_some_and(|signature| signature.no_alias)
}

fn has_no_alias_method_signature(property: &Place) -> bool {
    has_no_alias_function_signature(&property.identifier.type_)
}

fn load_global_name_for_hook_detection(binding: &NonLocalBinding) -> String {
    match binding {
        NonLocalBinding::ImportSpecifier { imported, .. } => imported.clone(),
        _ => binding.name().to_string(),
    }
}

// ---------------------------------------------------------------------------
// LValueMemoization
// ---------------------------------------------------------------------------

struct LValueMemoization<'a> {
    place: &'a Place,
    level: MemoizationLevel,
}

struct MemoizationInputs<'a> {
    lvalues: Vec<LValueMemoization<'a>>,
    rvalues: Vec<&'a Place>,
}

// ---------------------------------------------------------------------------
// Collecting operands from InstructionValue (equivalent to
// eachReactiveValueOperand / eachInstructionValueOperand)
// ---------------------------------------------------------------------------

/// Collects all operand places from an instruction value.
fn collect_operands(value: &InstructionValue) -> Vec<&Place> {
    let mut operands = Vec::new();
    match value {
        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            operands.push(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => operands.push(p),
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            operands.push(receiver);
            operands.push(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => operands.push(p),
                }
            }
        }
        InstructionValue::BinaryExpression { left, right, .. } => {
            operands.push(left);
            operands.push(right);
        }
        InstructionValue::UnaryExpression { value, .. } => {
            operands.push(value);
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            operands.push(place);
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => {
            operands.push(value);
        }
        InstructionValue::StoreGlobal { value, .. } => {
            operands.push(value);
        }
        InstructionValue::Destructure { value, .. } => {
            operands.push(value);
        }
        InstructionValue::PropertyLoad { object, .. } => {
            operands.push(object);
        }
        InstructionValue::PropertyStore { object, value, .. } => {
            operands.push(object);
            operands.push(value);
        }
        InstructionValue::PropertyDelete { object, .. } => {
            operands.push(object);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        } => {
            operands.push(object);
            operands.push(property);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => {
            operands.push(object);
            operands.push(property);
            operands.push(value);
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            operands.push(object);
            operands.push(property);
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            for prop in properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        if let ObjectPropertyKey::Computed(place) = &p.key {
                            operands.push(place);
                        }
                        operands.push(&p.place);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        operands.push(place);
                    }
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            for elem in elements {
                match elem {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => operands.push(p),
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
            if let JsxTag::Component(place) = tag {
                operands.push(place);
            }
            for prop in props {
                match prop {
                    JsxAttribute::Attribute { place, .. } => operands.push(place),
                    JsxAttribute::SpreadAttribute { argument } => operands.push(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    operands.push(child);
                }
            }
        }
        InstructionValue::JsxFragment { children, .. } => {
            for child in children {
                operands.push(child);
            }
        }
        InstructionValue::TypeCastExpression { value, .. }
        | InstructionValue::Await { value, .. } => {
            operands.push(value);
        }
        InstructionValue::GetIterator { collection, .. } => {
            operands.push(collection);
        }
        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => {
            operands.push(iterator);
            operands.push(collection);
        }
        InstructionValue::NextPropertyOf { value, .. } => {
            operands.push(value);
        }
        InstructionValue::PrefixUpdate { lvalue, value, .. }
        | InstructionValue::PostfixUpdate { lvalue, value, .. } => {
            operands.push(lvalue);
            operands.push(value);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            operands.push(test);
            operands.push(consequent);
            operands.push(alternate);
        }
        InstructionValue::LogicalExpression { left, right, .. } => {
            operands.push(left);
            operands.push(right);
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            for instr in instructions {
                if let Some(lvalue) = &instr.lvalue {
                    operands.push(lvalue);
                }
                operands.extend(collect_operands(&instr.value));
            }
            operands.extend(collect_operands(value));
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            operands.extend(collect_operands(value));
        }
        InstructionValue::ReactiveLogicalExpression { left, right, .. } => {
            operands.extend(collect_operands(left));
            operands.extend(collect_operands(right));
        }
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => {
            operands.extend(collect_operands(test));
            operands.extend(collect_operands(consequent));
            operands.extend(collect_operands(alternate));
        }
        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            operands.push(tag);
        }
        InstructionValue::TemplateLiteral { subexprs, .. } => {
            for expr in subexprs {
                operands.push(expr);
            }
        }
        InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. } => {
            // Nested functions — operands are captured by the lowered function.
            // For simplicity, we treat these as producing a new value (Memoized).
        }
        InstructionValue::FinishMemoize { decl, .. } => {
            operands.push(decl);
        }
        InstructionValue::RegExpLiteral { .. } => {}
        // No operands
        InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::Debugger { .. } => {}
    }
    operands
}

// ---------------------------------------------------------------------------
// computePatternLValues
// ---------------------------------------------------------------------------

fn compute_pattern_lvalues(pattern: &Pattern) -> Vec<LValueMemoization<'_>> {
    let mut lvalues = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(place) => {
                        lvalues.push(LValueMemoization {
                            place,
                            level: MemoizationLevel::Conditional,
                        });
                    }
                    ArrayElement::Spread(place) => {
                        lvalues.push(LValueMemoization {
                            place,
                            level: MemoizationLevel::Memoized,
                        });
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        lvalues.push(LValueMemoization {
                            place: &p.place,
                            level: MemoizationLevel::Conditional,
                        });
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        lvalues.push(LValueMemoization {
                            place,
                            level: MemoizationLevel::Memoized,
                        });
                    }
                }
            }
        }
    }
    lvalues
}

// ---------------------------------------------------------------------------
// computeMemoizationInputs
// ---------------------------------------------------------------------------

/// Determines the memoization level and lvalue/rvalue sets for a given
/// instruction value.
fn compute_memoization_inputs<'a>(
    value: &'a InstructionValue,
    lvalue: Option<&'a Place>,
    options: &MemoizationOptions,
    conditional_only_decls: &HashSet<DeclarationId>,
    conditional_fallback_decls: &HashSet<DeclarationId>,
) -> MemoizationInputs<'a> {
    match value {
        // Ternary: in upstream this is ConditionalExpression on ReactiveValue,
        // which is a nested value. In our Rust codebase, it's flattened to
        // a Ternary instruction with Place operands.
        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            // Only need to memoize if the rvalues are memoized
            MemoizationInputs {
                lvalues: lvalue
                    .map(|p| {
                        vec![LValueMemoization {
                            place: p,
                            level: MemoizationLevel::Conditional,
                        }]
                    })
                    .unwrap_or_default(),
                // Conditionals do not alias their test value; only consequent
                // and alternate are rvalues.
                rvalues: vec![consequent, alternate],
            }
        }

        InstructionValue::LogicalExpression { left, right, .. } => MemoizationInputs {
            lvalues: lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Conditional,
                    }]
                })
                .unwrap_or_default(),
            rvalues: vec![left, right],
        },
        InstructionValue::ReactiveSequenceExpression { .. }
        | InstructionValue::ReactiveOptionalExpression { .. }
        | InstructionValue::ReactiveLogicalExpression { .. }
        | InstructionValue::ReactiveConditionalExpression { .. } => MemoizationInputs {
            lvalues: lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Conditional,
                    }]
                })
                .unwrap_or_default(),
            rvalues: collect_operands(value),
        },

        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            let mut rvalues: Vec<&Place> = Vec::new();
            if let JsxTag::Component(place) = tag {
                rvalues.push(place);
            }
            for prop in props {
                match prop {
                    JsxAttribute::Attribute { place, .. } => rvalues.push(place),
                    JsxAttribute::SpreadAttribute { argument } => rvalues.push(argument),
                }
            }
            if let Some(children) = children {
                for child in children {
                    rvalues.push(child);
                }
            }
            let level = if options.memoize_jsx_elements {
                MemoizationLevel::Memoized
            } else {
                MemoizationLevel::Unmemoized
            };
            let include_lvalue = !lvalue.is_some_and(|p| {
                conditional_only_decls.contains(&p.identifier.declaration_id)
                    && !conditional_fallback_decls.contains(&p.identifier.declaration_id)
            });
            MemoizationInputs {
                lvalues: if include_lvalue {
                    lvalue
                        .map(|p| vec![LValueMemoization { place: p, level }])
                        .unwrap_or_default()
                } else {
                    vec![]
                },
                rvalues,
            }
        }

        InstructionValue::JsxFragment { children, .. } => {
            let level = if options.memoize_jsx_elements {
                MemoizationLevel::Memoized
            } else {
                MemoizationLevel::Unmemoized
            };
            let include_lvalue = !lvalue.is_some_and(|p| {
                conditional_only_decls.contains(&p.identifier.declaration_id)
                    && !conditional_fallback_decls.contains(&p.identifier.declaration_id)
            });
            MemoizationInputs {
                lvalues: if include_lvalue {
                    lvalue
                        .map(|p| vec![LValueMemoization { place: p, level }])
                        .unwrap_or_default()
                } else {
                    vec![]
                },
                rvalues: children.iter().collect(),
            }
        }

        // Instructions that always produce primitives
        InstructionValue::NextPropertyOf { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::ComputedDelete { .. }
        | InstructionValue::PropertyDelete { .. }
        | InstructionValue::LoadGlobal { .. }
        | InstructionValue::MetaProperty { .. }
        | InstructionValue::TemplateLiteral { .. }
        | InstructionValue::Primitive { .. }
        | InstructionValue::JSXText { .. }
        | InstructionValue::BinaryExpression { .. }
        | InstructionValue::UnaryExpression { .. } => {
            if options.force_memoize_primitives {
                let level = MemoizationLevel::Conditional;
                MemoizationInputs {
                    lvalues: lvalue
                        .map(|p| vec![LValueMemoization { place: p, level }])
                        .unwrap_or_default(),
                    rvalues: collect_operands(value),
                }
            } else {
                let level = MemoizationLevel::Never;
                MemoizationInputs {
                    lvalues: lvalue
                        .map(|p| vec![LValueMemoization { place: p, level }])
                        .unwrap_or_default(),
                    rvalues: vec![],
                }
            }
        }

        InstructionValue::Await { value: val, .. }
        | InstructionValue::TypeCastExpression { value: val, .. } => MemoizationInputs {
            lvalues: lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Conditional,
                    }]
                })
                .unwrap_or_default(),
            rvalues: vec![val],
        },

        InstructionValue::IteratorNext {
            iterator,
            collection,
            ..
        } => MemoizationInputs {
            lvalues: lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Conditional,
                    }]
                })
                .unwrap_or_default(),
            rvalues: vec![iterator, collection],
        },

        InstructionValue::GetIterator { collection, .. } => MemoizationInputs {
            lvalues: lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Conditional,
                    }]
                })
                .unwrap_or_default(),
            rvalues: vec![collection],
        },

        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            MemoizationInputs {
                lvalues: lvalue
                    .map(|p| {
                        vec![LValueMemoization {
                            place: p,
                            level: MemoizationLevel::Conditional,
                        }]
                    })
                    .unwrap_or_default(),
                rvalues: vec![place],
            }
        }

        InstructionValue::DeclareContext { lvalue: lv, .. } => {
            let mut lvalues = vec![LValueMemoization {
                place: &lv.place,
                level: MemoizationLevel::Memoized,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Unmemoized,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![],
            }
        }

        InstructionValue::DeclareLocal { lvalue: lv, .. } => {
            let mut lvalues = vec![LValueMemoization {
                place: &lv.place,
                level: MemoizationLevel::Unmemoized,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Unmemoized,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![],
            }
        }

        InstructionValue::PrefixUpdate {
            lvalue: lv,
            value: val,
            ..
        }
        | InstructionValue::PostfixUpdate {
            lvalue: lv,
            value: val,
            ..
        } => {
            let mut lvalues = vec![LValueMemoization {
                place: lv,
                level: MemoizationLevel::Conditional,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Conditional,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::StoreLocal {
            lvalue: lv,
            value: val,
            ..
        } => {
            let mut lvalues = vec![LValueMemoization {
                place: &lv.place,
                level: MemoizationLevel::Conditional,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Conditional,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::StoreContext {
            lvalue: lv,
            value: val,
            ..
        } => {
            // Should never be pruned
            let mut lvalues = vec![LValueMemoization {
                place: &lv.place,
                level: MemoizationLevel::Memoized,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Conditional,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::StoreGlobal { value: val, .. } => {
            let lvalues = lvalue
                .map(|p| {
                    vec![LValueMemoization {
                        place: p,
                        level: MemoizationLevel::Unmemoized,
                    }]
                })
                .unwrap_or_default();
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::Destructure {
            lvalue: lv,
            value: val,
            ..
        } => {
            let mut lvalues = Vec::new();
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Conditional,
                });
            }
            lvalues.extend(compute_pattern_lvalues(&lv.pattern));
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::ComputedLoad { object, .. }
        | InstructionValue::PropertyLoad { object, .. } => {
            let level = MemoizationLevel::Conditional;
            MemoizationInputs {
                lvalues: lvalue
                    .map(|p| vec![LValueMemoization { place: p, level }])
                    .unwrap_or_default(),
                rvalues: vec![object],
            }
        }

        InstructionValue::ComputedStore {
            object, value: val, ..
        } => {
            let mut lvalues = vec![LValueMemoization {
                place: object,
                level: MemoizationLevel::Conditional,
            }];
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Conditional,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: vec![val],
            }
        }

        InstructionValue::TaggedTemplateExpression { tag, .. } => {
            let mut lvalues: Vec<LValueMemoization<'_>> = Vec::new();
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Memoized,
                });
            }
            if has_no_alias_function_signature(&tag.identifier.type_) {
                return MemoizationInputs {
                    lvalues,
                    rvalues: vec![],
                };
            }
            let operands = collect_operands(value);
            for op in &operands {
                if is_mutable_operand(op) {
                    lvalues.push(LValueMemoization {
                        place: op,
                        level: MemoizationLevel::Memoized,
                    });
                }
            }
            MemoizationInputs {
                lvalues,
                rvalues: operands,
            }
        }

        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            // Nested functions close over outer bindings via `context`.
            // Track those captures as rvalue dependencies so escaping callbacks
            // keep required outer scopes alive.
            let rvalues: Vec<&Place> = lowered_func.func.context.iter().collect();
            let mut lvalues: Vec<LValueMemoization<'_>> = Vec::new();
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Memoized,
                });
            }
            for captured in &rvalues {
                if is_mutable_operand(captured) {
                    lvalues.push(LValueMemoization {
                        place: captured,
                        level: MemoizationLevel::Memoized,
                    });
                }
            }
            MemoizationInputs { lvalues, rvalues }
        }

        InstructionValue::CallExpression { callee, args, .. } => {
            let mut lvalues: Vec<LValueMemoization<'_>> = Vec::new();
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Memoized,
                });
            }
            if has_no_alias_function_signature(&callee.identifier.type_) {
                return MemoizationInputs {
                    lvalues,
                    rvalues: vec![],
                };
            }
            let mut all_operands: Vec<&Place> = Vec::new();
            all_operands.push(callee);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => all_operands.push(p),
                }
            }
            for (idx, op) in all_operands.iter().enumerate() {
                let is_arg = idx != 0;
                if is_arg
                    && matches!(
                        op.effect,
                        Effect::ConditionallyMutate | Effect::ConditionallyMutateIterator
                    )
                    && matches!(op.identifier.type_, Type::Primitive)
                {
                    continue;
                }
                if is_mutable_operand(op) {
                    lvalues.push(LValueMemoization {
                        place: op,
                        level: MemoizationLevel::Memoized,
                    });
                }
            }
            MemoizationInputs {
                lvalues,
                rvalues: all_operands,
            }
        }

        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            let mut lvalues: Vec<LValueMemoization<'_>> = Vec::new();
            if let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Memoized,
                });
            }
            if has_no_alias_method_signature(property) {
                return MemoizationInputs {
                    lvalues,
                    rvalues: vec![],
                };
            }
            let mut all_operands: Vec<&Place> = Vec::new();
            all_operands.push(receiver);
            all_operands.push(property);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => all_operands.push(p),
                }
            }
            for op in &all_operands {
                if is_mutable_operand(op) {
                    lvalues.push(LValueMemoization {
                        place: op,
                        level: MemoizationLevel::Memoized,
                    });
                }
            }
            MemoizationInputs {
                lvalues,
                rvalues: all_operands,
            }
        }

        // These instructions may produce new values which must be memoized if
        // reachable from a return value.
        InstructionValue::RegExpLiteral { .. }
        | InstructionValue::ArrayExpression { .. }
        | InstructionValue::NewExpression { .. }
        | InstructionValue::ObjectExpression { .. }
        | InstructionValue::PropertyStore { .. } => {
            let operands = collect_operands(value);
            let mut lvalues: Vec<LValueMemoization<'_>> = operands
                .iter()
                .filter(|op| is_mutable_operand(op))
                .map(|op| LValueMemoization {
                    place: op,
                    level: MemoizationLevel::Memoized,
                })
                .collect();
            let include_lvalue = !lvalue.is_some_and(|p| {
                if !conditional_only_decls.contains(&p.identifier.declaration_id) {
                    return false;
                }
                match value {
                    InstructionValue::ArrayExpression { .. } => true,
                    InstructionValue::ObjectExpression { properties, .. } => {
                        !properties.iter().any(|property| {
                            matches!(
                                property,
                                ObjectPropertyOrSpread::Property(ObjectProperty {
                                    type_: ObjectPropertyType::Method,
                                    ..
                                })
                            )
                        })
                    }
                    _ => false,
                }
            });
            if include_lvalue && let Some(p) = lvalue {
                lvalues.push(LValueMemoization {
                    place: p,
                    level: MemoizationLevel::Memoized,
                });
            }
            MemoizationInputs {
                lvalues,
                rvalues: operands,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Collect dependencies (visitor over ReactiveFunction tree)
// ---------------------------------------------------------------------------

/// Identify declaration IDs that are only consumed as operands of flattened
/// logical/ternary expressions. Upstream keeps nested values inline here; our
/// flattened HIR introduces temporaries that should remain conditional.
fn collect_conditional_only_declarations(func: &ReactiveFunction) -> HashSet<DeclarationId> {
    // Defaults lowered from patterns (`x === undefined ? default : x`) should not
    // be treated as conditional-only; upstream memoizes these defaults.
    let default_conditional_decls = collect_default_conditional_declarations(func);

    #[derive(Clone, Copy)]
    enum UseKind {
        Normal,
        LogicalConditional,
        TernaryConditional,
        AliasTo(DeclarationId),
    }

    fn record_use(
        uses: &mut HashMap<DeclarationId, Vec<UseKind>>,
        decl: DeclarationId,
        use_kind: UseKind,
    ) {
        uses.entry(decl).or_default().push(use_kind);
    }

    fn walk_terminal(terminal: &ReactiveTerminal, uses: &mut HashMap<DeclarationId, Vec<UseKind>>) {
        match terminal {
            ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
                record_use(uses, value.identifier.declaration_id, UseKind::Normal);
            }
            ReactiveTerminal::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                walk_block(consequent, uses);
                if let Some(alt) = alternate {
                    walk_block(alt, uses);
                }
            }
            ReactiveTerminal::Switch { test, cases, .. } => {
                record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                for case in cases {
                    if let Some(test) = &case.test {
                        record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                    }
                    if let Some(block) = &case.block {
                        walk_block(block, uses);
                    }
                }
            }
            ReactiveTerminal::DoWhile {
                loop_block, test, ..
            }
            | ReactiveTerminal::While {
                test, loop_block, ..
            } => {
                record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                walk_block(loop_block, uses);
            }
            ReactiveTerminal::For {
                init,
                test,
                update,
                loop_block,
                ..
            } => {
                record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                walk_block(init, uses);
                if let Some(update) = update {
                    walk_block(update, uses);
                }
                walk_block(loop_block, uses);
            }
            ReactiveTerminal::ForOf {
                init,
                test,
                loop_block,
                ..
            } => {
                record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                walk_block(init, uses);
                walk_block(loop_block, uses);
            }
            ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                walk_block(init, uses);
                walk_block(loop_block, uses);
            }
            ReactiveTerminal::Label { block, .. } => {
                walk_block(block, uses);
            }
            ReactiveTerminal::Try {
                block,
                handler_binding,
                handler,
                ..
            } => {
                if let Some(binding) = handler_binding {
                    record_use(uses, binding.identifier.declaration_id, UseKind::Normal);
                }
                walk_block(block, uses);
                walk_block(handler, uses);
            }
            ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        }
    }

    fn walk_block(block: &ReactiveBlock, uses: &mut HashMap<DeclarationId, Vec<UseKind>>) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. }
                    | InstructionValue::StoreContext { lvalue, value, .. } => {
                        record_use(
                            uses,
                            value.identifier.declaration_id,
                            UseKind::AliasTo(lvalue.place.identifier.declaration_id),
                        );
                    }
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        let use_kind = instr
                            .lvalue
                            .as_ref()
                            .map(|lvalue| UseKind::AliasTo(lvalue.identifier.declaration_id))
                            .unwrap_or(UseKind::Normal);
                        record_use(uses, place.identifier.declaration_id, use_kind);
                    }
                    InstructionValue::TypeCastExpression { value, .. } => {
                        let use_kind = instr
                            .lvalue
                            .as_ref()
                            .map(|lvalue| UseKind::AliasTo(lvalue.identifier.declaration_id))
                            .unwrap_or(UseKind::Normal);
                        record_use(uses, value.identifier.declaration_id, use_kind);
                    }
                    InstructionValue::LogicalExpression { left, right, .. } => {
                        record_use(
                            uses,
                            left.identifier.declaration_id,
                            UseKind::LogicalConditional,
                        );
                        record_use(
                            uses,
                            right.identifier.declaration_id,
                            UseKind::LogicalConditional,
                        );
                    }
                    InstructionValue::Ternary {
                        test,
                        consequent,
                        alternate,
                        ..
                    } => {
                        record_use(uses, test.identifier.declaration_id, UseKind::Normal);
                        record_use(
                            uses,
                            consequent.identifier.declaration_id,
                            UseKind::TernaryConditional,
                        );
                        record_use(
                            uses,
                            alternate.identifier.declaration_id,
                            UseKind::TernaryConditional,
                        );
                    }
                    _ => {
                        for operand in collect_operands(&instr.value) {
                            record_use(uses, operand.identifier.declaration_id, UseKind::Normal);
                        }
                    }
                },
                ReactiveStatement::Terminal(term_stmt) => {
                    walk_terminal(&term_stmt.terminal, uses);
                }
                ReactiveStatement::Scope(scope_block) => {
                    walk_block(&scope_block.instructions, uses);
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    walk_block(&scope_block.instructions, uses);
                }
            }
        }
    }

    let mut uses: HashMap<DeclarationId, Vec<UseKind>> = HashMap::new();
    walk_block(&func.body, &mut uses);

    let mut conditional_only = HashSet::new();
    for (&decl, use_kinds) in &uses {
        if default_conditional_decls.contains(&decl) {
            continue;
        }
        if !use_kinds.is_empty()
            && use_kinds.iter().all(|use_kind| {
                matches!(
                    use_kind,
                    UseKind::LogicalConditional | UseKind::TernaryConditional
                )
            })
        {
            conditional_only.insert(decl);
        }
    }

    let mut ternary_rooted = HashSet::new();
    let mut changed = true;
    while changed {
        changed = false;
        for (&decl, use_kinds) in &uses {
            if ternary_rooted.contains(&decl) || default_conditional_decls.contains(&decl) {
                continue;
            }
            let has_direct_ternary_use = use_kinds
                .iter()
                .any(|use_kind| matches!(use_kind, UseKind::TernaryConditional));
            if !use_kinds.is_empty()
                && use_kinds.iter().all(|use_kind| match use_kind {
                    UseKind::TernaryConditional => true,
                    UseKind::AliasTo(target) => {
                        has_direct_ternary_use || ternary_rooted.contains(target)
                    }
                    UseKind::LogicalConditional => false,
                    UseKind::Normal => false,
                })
            {
                changed |= ternary_rooted.insert(decl);
            }
        }
    }

    conditional_only.extend(ternary_rooted);
    conditional_only
}

/// Find declaration IDs used as the non-primitive branch of a ternary
/// fallback (e.g. `cond ? <JSX /> : null`). These are the conditional JSX
/// temporaries that should remain memoizable to match upstream output.
fn collect_conditional_fallback_declarations(func: &ReactiveFunction) -> HashSet<DeclarationId> {
    fn walk_block(
        block: &ReactiveBlock,
        primitive_decls: &mut HashSet<DeclarationId>,
        fallback_decls: &mut HashSet<DeclarationId>,
    ) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if let Some(lvalue) = instr.lvalue.as_ref()
                        && matches!(
                            instr.value,
                            InstructionValue::Primitive { .. } | InstructionValue::JSXText { .. }
                        )
                    {
                        primitive_decls.insert(lvalue.identifier.declaration_id);
                    }

                    if let InstructionValue::Ternary {
                        consequent,
                        alternate,
                        ..
                    } = &instr.value
                    {
                        let cons_decl = consequent.identifier.declaration_id;
                        let alt_decl = alternate.identifier.declaration_id;
                        let cons_primitive = primitive_decls.contains(&cons_decl)
                            || matches!(consequent.identifier.type_, Type::Primitive);
                        let alt_primitive = primitive_decls.contains(&alt_decl)
                            || matches!(alternate.identifier.type_, Type::Primitive);
                        if cons_primitive != alt_primitive {
                            if cons_primitive {
                                fallback_decls.insert(alt_decl);
                            } else {
                                fallback_decls.insert(cons_decl);
                            }
                        }
                    }

                    if let InstructionValue::LogicalExpression { left, right, .. } = &instr.value {
                        let left_decl = left.identifier.declaration_id;
                        let right_decl = right.identifier.declaration_id;
                        let left_primitive = primitive_decls.contains(&left_decl)
                            || matches!(left.identifier.type_, Type::Primitive);
                        let right_primitive = primitive_decls.contains(&right_decl)
                            || matches!(right.identifier.type_, Type::Primitive);
                        if left_primitive != right_primitive {
                            if left_primitive {
                                fallback_decls.insert(right_decl);
                            } else {
                                fallback_decls.insert(left_decl);
                            }
                        }
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    walk_terminal(&term_stmt.terminal, primitive_decls, fallback_decls);
                }
                ReactiveStatement::Scope(scope_block) => {
                    walk_block(&scope_block.instructions, primitive_decls, fallback_decls);
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    walk_block(&scope_block.instructions, primitive_decls, fallback_decls);
                }
            }
        }
    }

    fn walk_terminal(
        terminal: &ReactiveTerminal,
        primitive_decls: &mut HashSet<DeclarationId>,
        fallback_decls: &mut HashSet<DeclarationId>,
    ) {
        match terminal {
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                walk_block(consequent, primitive_decls, fallback_decls);
                if let Some(alt) = alternate {
                    walk_block(alt, primitive_decls, fallback_decls);
                }
            }
            ReactiveTerminal::Switch { cases, .. } => {
                for case in cases {
                    if let Some(block) = &case.block {
                        walk_block(block, primitive_decls, fallback_decls);
                    }
                }
            }
            ReactiveTerminal::For {
                init,
                update,
                loop_block,
                ..
            } => {
                walk_block(init, primitive_decls, fallback_decls);
                if let Some(upd) = update {
                    walk_block(upd, primitive_decls, fallback_decls);
                }
                walk_block(loop_block, primitive_decls, fallback_decls);
            }
            ReactiveTerminal::ForOf {
                init, loop_block, ..
            }
            | ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                walk_block(init, primitive_decls, fallback_decls);
                walk_block(loop_block, primitive_decls, fallback_decls);
            }
            ReactiveTerminal::DoWhile { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. }
            | ReactiveTerminal::Label {
                block: loop_block, ..
            } => {
                walk_block(loop_block, primitive_decls, fallback_decls);
            }
            ReactiveTerminal::Try { block, handler, .. } => {
                walk_block(block, primitive_decls, fallback_decls);
                walk_block(handler, primitive_decls, fallback_decls);
            }
            ReactiveTerminal::Break { .. }
            | ReactiveTerminal::Continue { .. }
            | ReactiveTerminal::Return { .. }
            | ReactiveTerminal::Throw { .. } => {}
        }
    }

    let mut primitive_decls: HashSet<DeclarationId> = HashSet::new();
    let mut fallback_decls: HashSet<DeclarationId> = HashSet::new();
    walk_block(&func.body, &mut primitive_decls, &mut fallback_decls);
    fallback_decls
}

fn collect_default_conditional_declarations(func: &ReactiveFunction) -> HashSet<DeclarationId> {
    let mut undefined_decls: HashSet<DeclarationId> = HashSet::new();
    let mut strict_eq_tests: HashMap<DeclarationId, (DeclarationId, DeclarationId)> =
        HashMap::new();
    let mut ternary_inputs: Vec<(DeclarationId, DeclarationId, DeclarationId)> = Vec::new();

    fn walk_block(
        block: &ReactiveBlock,
        undefined_decls: &mut HashSet<DeclarationId>,
        strict_eq_tests: &mut HashMap<DeclarationId, (DeclarationId, DeclarationId)>,
        ternary_inputs: &mut Vec<(DeclarationId, DeclarationId, DeclarationId)>,
    ) {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    let Some(lvalue) = instr.lvalue.as_ref() else {
                        continue;
                    };
                    let lvalue_decl = lvalue.identifier.declaration_id;
                    match &instr.value {
                        InstructionValue::Primitive {
                            value: PrimitiveValue::Undefined,
                            ..
                        } => {
                            undefined_decls.insert(lvalue_decl);
                        }
                        InstructionValue::BinaryExpression {
                            operator,
                            left,
                            right,
                            ..
                        } => {
                            if matches!(operator, BinaryOperator::StrictEq | BinaryOperator::Eq) {
                                strict_eq_tests.insert(
                                    lvalue_decl,
                                    (
                                        left.identifier.declaration_id,
                                        right.identifier.declaration_id,
                                    ),
                                );
                            }
                        }
                        InstructionValue::Ternary {
                            test,
                            consequent,
                            alternate,
                            ..
                        } => {
                            ternary_inputs.push((
                                test.identifier.declaration_id,
                                consequent.identifier.declaration_id,
                                alternate.identifier.declaration_id,
                            ));
                        }
                        _ => {}
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => match &term_stmt.terminal {
                    ReactiveTerminal::If {
                        consequent,
                        alternate,
                        ..
                    } => {
                        walk_block(consequent, undefined_decls, strict_eq_tests, ternary_inputs);
                        if let Some(alt) = alternate {
                            walk_block(alt, undefined_decls, strict_eq_tests, ternary_inputs);
                        }
                    }
                    ReactiveTerminal::Switch { cases, .. } => {
                        for case in cases {
                            if let Some(block) = &case.block {
                                walk_block(block, undefined_decls, strict_eq_tests, ternary_inputs);
                            }
                        }
                    }
                    ReactiveTerminal::DoWhile { loop_block, .. }
                    | ReactiveTerminal::While { loop_block, .. } => {
                        walk_block(loop_block, undefined_decls, strict_eq_tests, ternary_inputs);
                    }
                    ReactiveTerminal::For {
                        init,
                        update,
                        loop_block,
                        ..
                    } => {
                        walk_block(init, undefined_decls, strict_eq_tests, ternary_inputs);
                        if let Some(update) = update {
                            walk_block(update, undefined_decls, strict_eq_tests, ternary_inputs);
                        }
                        walk_block(loop_block, undefined_decls, strict_eq_tests, ternary_inputs);
                    }
                    ReactiveTerminal::ForOf {
                        init, loop_block, ..
                    }
                    | ReactiveTerminal::ForIn {
                        init, loop_block, ..
                    } => {
                        walk_block(init, undefined_decls, strict_eq_tests, ternary_inputs);
                        walk_block(loop_block, undefined_decls, strict_eq_tests, ternary_inputs);
                    }
                    ReactiveTerminal::Label { block, .. } => {
                        walk_block(block, undefined_decls, strict_eq_tests, ternary_inputs);
                    }
                    ReactiveTerminal::Try { block, handler, .. } => {
                        walk_block(block, undefined_decls, strict_eq_tests, ternary_inputs);
                        walk_block(handler, undefined_decls, strict_eq_tests, ternary_inputs);
                    }
                    ReactiveTerminal::Break { .. }
                    | ReactiveTerminal::Continue { .. }
                    | ReactiveTerminal::Return { .. }
                    | ReactiveTerminal::Throw { .. } => {}
                },
                ReactiveStatement::Scope(scope_block) => {
                    walk_block(
                        &scope_block.instructions,
                        undefined_decls,
                        strict_eq_tests,
                        ternary_inputs,
                    );
                }
                ReactiveStatement::PrunedScope(scope_block) => {
                    walk_block(
                        &scope_block.instructions,
                        undefined_decls,
                        strict_eq_tests,
                        ternary_inputs,
                    );
                }
            }
        }
    }

    walk_block(
        &func.body,
        &mut undefined_decls,
        &mut strict_eq_tests,
        &mut ternary_inputs,
    );

    let mut defaults = HashSet::new();
    for (test_decl, consequent_decl, alternate_decl) in ternary_inputs {
        if let Some((left_decl, right_decl)) = strict_eq_tests.get(&test_decl)
            && ((*left_decl == alternate_decl && undefined_decls.contains(right_decl))
                || (*right_decl == alternate_decl && undefined_decls.contains(left_decl)))
        {
            defaults.insert(consequent_decl);
        }
    }
    defaults
}

/// Visit a single instruction value for memoization, populating the state graph.
fn visit_value_for_memoization(
    state: &mut State,
    instr_id: InstructionId,
    value: &InstructionValue,
    lvalue: Option<&Place>,
    options: &MemoizationOptions,
    conditional_only_decls: &HashSet<DeclarationId>,
    conditional_fallback_decls: &HashSet<DeclarationId>,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) {
    let aliasing = compute_memoization_inputs(
        value,
        lvalue,
        options,
        conditional_only_decls,
        conditional_fallback_decls,
    );
    let debug_hooks = std::env::var("DEBUG_PRUNE_HOOKS").is_ok();
    let debug_mutable_lvalues = std::env::var("DEBUG_MUTABLE_LVALUES").is_ok();

    // Associate all the rvalues with the instruction's scope
    for operand in &aliasing.rvalues {
        for operand_id in state.resolve_all(operand.identifier.declaration_id) {
            state.visit_operand(instr_id, operand, operand_id);
        }
    }

    // Add the operands as dependencies of all lvalues
    for lv in &aliasing.lvalues {
        if debug_mutable_lvalues
            && matches!(
                lv.place.effect,
                Effect::Capture
                    | Effect::Store
                    | Effect::Mutate
                    | Effect::ConditionallyMutate
                    | Effect::ConditionallyMutateIterator
            )
        {
            eprintln!(
                "[PRUNE_MUTABLE] instr={} kind={} decl={} effect={:?} type={:?} level={:?}",
                instr_id.0,
                instruction_value_kind(value),
                lv.place.identifier.declaration_id.0,
                lv.place.effect,
                lv.place.identifier.type_,
                lv.level
            );
        }
        let lvalue_id = state.resolve_unique(lv.place.identifier.declaration_id);
        // Ensure the identifier node exists
        state.ensure_identifier(lvalue_id);
        // We need to collect deps and do updates carefully due to borrow checker
        let rvalue_ids: Vec<DeclarationId> = aliasing
            .rvalues
            .iter()
            .flat_map(|op| state.resolve_all(op.identifier.declaration_id).into_iter())
            .filter(|&op_id| op_id != lvalue_id)
            .collect();

        let node = state.identifiers.get_mut(&lvalue_id).unwrap();
        node.level = join_aliases(node.level, lv.level);
        for op_id in rvalue_ids {
            node.dependencies.insert(op_id);
        }

        // Visit lvalue operand to associate with scope
        state.visit_operand(instr_id, lv.place, lvalue_id);
    }

    // Handle LoadLocal definitions
    if let InstructionValue::LoadLocal { place, .. } = value
        && let Some(lv) = lvalue
    {
        state.insert_definition(
            lv.identifier.declaration_id,
            place.identifier.declaration_id,
        );
    }

    // Handle hook calls -- mark arguments as escaping
    match value {
        InstructionValue::CallExpression { callee, args, .. } => {
            let is_hook = is_hook_callee(&callee.identifier, id_to_name, load_source);
            let no_alias = has_no_alias_function_signature(&callee.identifier.type_);
            if debug_hooks {
                eprintln!(
                    "[PRUNE_HOOK] Call callee_id={} name={:?} mapped={:?} hook={} no_alias={}",
                    callee.identifier.id.0,
                    callee
                        .identifier
                        .name
                        .as_ref()
                        .map(|n| n.value().to_string()),
                    id_to_name.get(&callee.identifier.id),
                    is_hook,
                    no_alias
                );
            }
            if is_hook && !no_alias {
                for arg in args {
                    let place = match arg {
                        Argument::Place(p) | Argument::Spread(p) => p,
                    };
                    state
                        .escaping_values
                        .insert(place.identifier.declaration_id);
                }
            }
        }
        InstructionValue::MethodCall { property, args, .. } => {
            let is_hook = is_hook_callee(&property.identifier, id_to_name, load_source);
            let no_alias = has_no_alias_function_signature(&property.identifier.type_);
            if debug_hooks {
                eprintln!(
                    "[PRUNE_HOOK] Method prop_id={} name={:?} mapped={:?} hook={} no_alias={}",
                    property.identifier.id.0,
                    property
                        .identifier
                        .name
                        .as_ref()
                        .map(|n| n.value().to_string()),
                    id_to_name.get(&property.identifier.id),
                    is_hook,
                    no_alias
                );
            }
            if is_hook && !no_alias {
                for arg in args {
                    let place = match arg {
                        Argument::Place(p) | Argument::Spread(p) => p,
                    };
                    state
                        .escaping_values
                        .insert(place.identifier.declaration_id);
                }
            }
        }
        _ => {}
    }
}

/// Recursively visit the reactive function tree, collecting dependencies.
fn collect_dependencies_block(
    state: &mut State,
    block: &ReactiveBlock,
    active_scopes: &[ScopeId],
    options: &MemoizationOptions,
    conditional_only_decls: &HashSet<DeclarationId>,
    conditional_fallback_decls: &HashSet<DeclarationId>,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                visit_value_for_memoization(
                    state,
                    instr.id,
                    &instr.value,
                    instr.lvalue.as_ref(),
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_dependencies_terminal(
                    state,
                    term_stmt,
                    active_scopes,
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
            ReactiveStatement::Scope(scope_block) => {
                // If a scope reassigns variables, set the chain of active
                // scopes as dependencies of those variables.
                for reassignment in &scope_block.scope.reassignments {
                    let node = state.ensure_identifier(reassignment.declaration_id);
                    for &scope_id in active_scopes {
                        node.scopes.insert(scope_id);
                    }
                    node.scopes.insert(scope_block.scope.id);
                }

                let mut new_scopes = active_scopes.to_vec();
                new_scopes.push(scope_block.scope.id);
                collect_dependencies_block(
                    state,
                    &scope_block.instructions,
                    &new_scopes,
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_dependencies_block(
                    state,
                    &scope_block.instructions,
                    active_scopes,
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
        }
    }
}

fn collect_dependencies_terminal(
    state: &mut State,
    term_stmt: &ReactiveTerminalStatement,
    active_scopes: &[ScopeId],
    options: &MemoizationOptions,
    conditional_only_decls: &HashSet<DeclarationId>,
    conditional_fallback_decls: &HashSet<DeclarationId>,
    id_to_name: &HashMap<IdentifierId, String>,
    load_source: &HashMap<IdentifierId, IdentifierId>,
) {
    let terminal = &term_stmt.terminal;
    match terminal {
        ReactiveTerminal::Return { value, .. } => {
            state
                .escaping_values
                .insert(value.identifier.declaration_id);

            // If the return is within a scope, those scopes must be considered
            // dependencies of the returned value.
            let node = state.ensure_identifier(value.identifier.declaration_id);
            for &scope_id in active_scopes {
                node.scopes.insert(scope_id);
            }
        }
        ReactiveTerminal::Throw { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_dependencies_block(
                state,
                consequent,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
            if let Some(alt) = alternate {
                collect_dependencies_block(
                    state,
                    alt,
                    active_scopes,
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_dependencies_block(
                        state,
                        block,
                        active_scopes,
                        options,
                        conditional_only_decls,
                        conditional_fallback_decls,
                        id_to_name,
                        load_source,
                    );
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_dependencies_block(
                state,
                loop_block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_dependencies_block(
                state,
                init,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
            if let Some(upd) = update {
                collect_dependencies_block(
                    state,
                    upd,
                    active_scopes,
                    options,
                    conditional_only_decls,
                    conditional_fallback_decls,
                    id_to_name,
                    load_source,
                );
            }
            collect_dependencies_block(
                state,
                loop_block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            collect_dependencies_block(
                state,
                init,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
            collect_dependencies_block(
                state,
                loop_block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_dependencies_block(
                state,
                init,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
            collect_dependencies_block(
                state,
                loop_block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_dependencies_block(
                state,
                block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_dependencies_block(
                state,
                block,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
            collect_dependencies_block(
                state,
                handler,
                active_scopes,
                options,
                conditional_only_decls,
                conditional_fallback_decls,
                id_to_name,
                load_source,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Compute memoized identifiers
// ---------------------------------------------------------------------------

/// Walk the graph from escaping values to determine which identifiers
/// should be memoized.
fn compute_memoized_identifiers(state: &mut State) -> HashSet<DeclarationId> {
    let mut memoized = HashSet::new();
    let escaping: Vec<DeclarationId> = state.escaping_values.iter().copied().collect();

    for id in escaping {
        visit_identifier(state, &mut memoized, id, false);
    }

    memoized
}

/// Visit an identifier, optionally forcing it to be memoized. Returns whether
/// the identifier ended up memoized.
fn visit_identifier(
    state: &mut State,
    memoized: &mut HashSet<DeclarationId>,
    id: DeclarationId,
    force_memoize: bool,
) -> bool {
    let node = match state.identifiers.get_mut(&id) {
        Some(n) => n,
        None => return false,
    };
    if node.seen {
        return node.memoized;
    }
    node.seen = true;
    node.memoized = false;

    // Collect dependencies before recursive calls
    let deps: Vec<DeclarationId> = node.dependencies.iter().copied().collect();
    let level = node.level;

    // Visit dependencies
    let mut has_memoized_dependency = false;
    for dep in deps {
        let is_dep_memoized = visit_identifier(state, memoized, dep, false);
        has_memoized_dependency |= is_dep_memoized;
    }

    let should_memoize = level == MemoizationLevel::Memoized
        || (level == MemoizationLevel::Conditional && (has_memoized_dependency || force_memoize))
        || (level == MemoizationLevel::Unmemoized && force_memoize);

    if should_memoize {
        if let Some(node) = state.identifiers.get_mut(&id) {
            node.memoized = true;
        }
        memoized.insert(id);

        // Force memoize scope dependencies
        let scope_ids: Vec<ScopeId> = state
            .identifiers
            .get(&id)
            .map(|n| n.scopes.iter().copied().collect())
            .unwrap_or_default();
        for scope_id in scope_ids {
            force_memoize_scope_dependencies(state, memoized, scope_id);
        }
    }

    state.identifiers.get(&id).is_some_and(|n| n.memoized)
}

/// Force all the scope's optionally-memoizable dependencies to be memoized.
fn force_memoize_scope_dependencies(
    state: &mut State,
    memoized: &mut HashSet<DeclarationId>,
    scope_id: ScopeId,
) {
    let deps = match state.scopes.get_mut(&scope_id) {
        Some(node) => {
            if node.seen {
                return;
            }
            node.seen = true;
            node.dependencies.clone()
        }
        None => return,
    };

    for dep in deps {
        visit_identifier(state, memoized, dep, true);
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Prune scopes
// ---------------------------------------------------------------------------

/// Transform the block in-place, pruning scopes whose outputs are not memoized
/// and handling FinishMemoize pruned flags.
fn prune_scopes_block(
    block: &mut ReactiveBlock,
    memoized: &HashSet<DeclarationId>,
    definitions: &HashMap<DeclarationId, HashSet<DeclarationId>>,
    pruned_scopes: &mut HashSet<ScopeId>,
    reassignments: &mut HashMap<
        DeclarationId,
        HashSet<(IdentifierId, DeclarationId, Option<ScopeId>)>,
    >,
) {
    let mut i = 0;
    while i < block.len() {
        match &mut block[i] {
            ReactiveStatement::Instruction(instr) => {
                // Track reassignments for FinishMemoize
                match &instr.value {
                    InstructionValue::StoreLocal { lvalue, value, .. } => {
                        if lvalue.kind == InstructionKind::Reassign {
                            let ids = reassignments
                                .entry(lvalue.place.identifier.declaration_id)
                                .or_default();
                            ids.insert((
                                value.identifier.id,
                                value.identifier.declaration_id,
                                value.identifier.scope.as_ref().map(|s| s.id),
                            ));
                        }
                    }
                    InstructionValue::LoadLocal { place, .. } => {
                        if let Some(lv) = &instr.lvalue
                            && place.identifier.scope.is_some()
                            && lv.identifier.scope.is_none()
                        {
                            let ids = reassignments
                                .entry(lv.identifier.declaration_id)
                                .or_default();
                            ids.insert((
                                place.identifier.id,
                                place.identifier.declaration_id,
                                place.identifier.scope.as_ref().map(|s| s.id),
                            ));
                        }
                    }
                    InstructionValue::FinishMemoize { decl, .. } => {
                        let decl_scope = decl.identifier.scope.as_ref().map(|s| s.id);
                        let should_prune = if decl_scope.is_none() {
                            // If the manual memo was inlined, check reassignments
                            let decls = reassignments.get(&decl.identifier.declaration_id);
                            match decls {
                                Some(decl_set) => decl_set.iter().all(|(_id, _decl_id, scope)| {
                                    scope.is_none()
                                        || scope.is_some_and(|s| pruned_scopes.contains(&s))
                                }),
                                None => {
                                    // No reassignments found, check the decl itself
                                    decl_scope.is_none()
                                        || decl_scope.is_some_and(|s| pruned_scopes.contains(&s))
                                }
                            }
                        } else {
                            decl_scope.is_some_and(|s| pruned_scopes.contains(&s))
                        };

                        if should_prune {
                            // Set pruned = true on the FinishMemoize
                            if let ReactiveStatement::Instruction(instr) = &mut block[i]
                                && let InstructionValue::FinishMemoize { pruned, .. } =
                                    &mut instr.value
                            {
                                *pruned = true;
                            }
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                prune_scopes_terminal(
                    &mut term_stmt.terminal,
                    memoized,
                    definitions,
                    pruned_scopes,
                    reassignments,
                );
                i += 1;
            }
            ReactiveStatement::Scope(_) => {
                // We need to take the scope out temporarily to inspect it
                let stmt = block.remove(i);
                if let ReactiveStatement::Scope(mut scope_block) = stmt {
                    // Recurse into the scope's instructions first
                    prune_scopes_block(
                        &mut scope_block.instructions,
                        memoized,
                        definitions,
                        pruned_scopes,
                        reassignments,
                    );

                    // Check if scope should be kept
                    let is_empty = scope_block.scope.declarations.is_empty()
                        && scope_block.scope.reassignments.is_empty();
                    let has_early_return = scope_block.scope.early_return_value.is_some();

                    if is_empty || has_early_return {
                        // Keep empty scopes (let them be pruned later by
                        // PruneUnusedScopes after PropagateEarlyReturns).
                        // Also keep scopes with early return values.
                        block.insert(i, ReactiveStatement::Scope(scope_block));
                        i += 1;
                    } else {
                        let has_memoized_output =
                            scope_block.scope.declarations.values().any(|decl| {
                                declaration_is_memoized(
                                    decl.identifier.declaration_id,
                                    memoized,
                                    definitions,
                                )
                            }) || scope_block.scope.reassignments.iter().any(|ident| {
                                declaration_is_memoized(ident.declaration_id, memoized, definitions)
                            });
                        let scope_decl_ids: HashSet<DeclarationId> = scope_block
                            .scope
                            .declarations
                            .values()
                            .map(|decl| decl.identifier.declaration_id)
                            .collect();
                        let feeds_conditional_scope =
                            scope_feeds_flattened_conditional_dependency(block, i, &scope_decl_ids);
                        if has_memoized_output || feeds_conditional_scope {
                            if feeds_conditional_scope && !has_memoized_output {
                                debug_scope_prune(
                                    &scope_block.scope,
                                    "prune_non_escaping_scopes",
                                    "keep-conditional-dependency",
                                );
                            }
                            block.insert(i, ReactiveStatement::Scope(scope_block));
                            i += 1;
                        } else {
                            // Prune: replace scope with its instructions
                            debug_scope_prune(
                                &scope_block.scope,
                                "prune_non_escaping_scopes",
                                "non-escaping",
                            );
                            pruned_scopes.insert(scope_block.scope.id);
                            let instructions = scope_block.instructions;
                            let count = instructions.len();
                            for (j, instr) in instructions.into_iter().enumerate() {
                                block.insert(i + j, instr);
                            }
                            // Don't increment i -- re-process newly inserted items
                            // But if count is 0 we still need to move forward
                            if count == 0 {
                                // Nothing was inserted, just continue
                            }
                            // Continue without incrementing -- the loop will
                            // process the newly-inserted statements
                        }
                    }
                } else {
                    unreachable!();
                }
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                prune_scopes_block(
                    &mut scope_block.instructions,
                    memoized,
                    definitions,
                    pruned_scopes,
                    reassignments,
                );
                i += 1;
            }
        }
    }
}

fn prune_scopes_terminal(
    terminal: &mut ReactiveTerminal,
    memoized: &HashSet<DeclarationId>,
    definitions: &HashMap<DeclarationId, HashSet<DeclarationId>>,
    pruned_scopes: &mut HashSet<ScopeId>,
    reassignments: &mut HashMap<
        DeclarationId,
        HashSet<(IdentifierId, DeclarationId, Option<ScopeId>)>,
    >,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            prune_scopes_block(
                consequent,
                memoized,
                definitions,
                pruned_scopes,
                reassignments,
            );
            if let Some(alt) = alternate {
                prune_scopes_block(alt, memoized, definitions, pruned_scopes, reassignments);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases.iter_mut() {
                if let Some(block) = &mut case.block {
                    prune_scopes_block(block, memoized, definitions, pruned_scopes, reassignments);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            prune_scopes_block(
                loop_block,
                memoized,
                definitions,
                pruned_scopes,
                reassignments,
            );
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            prune_scopes_block(init, memoized, definitions, pruned_scopes, reassignments);
            if let Some(upd) = update {
                prune_scopes_block(upd, memoized, definitions, pruned_scopes, reassignments);
            }
            prune_scopes_block(
                loop_block,
                memoized,
                definitions,
                pruned_scopes,
                reassignments,
            );
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            prune_scopes_block(init, memoized, definitions, pruned_scopes, reassignments);
            prune_scopes_block(
                loop_block,
                memoized,
                definitions,
                pruned_scopes,
                reassignments,
            );
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            prune_scopes_block(init, memoized, definitions, pruned_scopes, reassignments);
            prune_scopes_block(
                loop_block,
                memoized,
                definitions,
                pruned_scopes,
                reassignments,
            );
        }
        ReactiveTerminal::Label { block, .. } => {
            prune_scopes_block(block, memoized, definitions, pruned_scopes, reassignments);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            prune_scopes_block(block, memoized, definitions, pruned_scopes, reassignments);
            prune_scopes_block(handler, memoized, definitions, pruned_scopes, reassignments);
        }
        ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. }
        | ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. } => {}
    }
}

fn declaration_is_memoized(
    decl: DeclarationId,
    memoized: &HashSet<DeclarationId>,
    definitions: &HashMap<DeclarationId, HashSet<DeclarationId>>,
) -> bool {
    if memoized.contains(&decl) {
        return true;
    }
    let mut stack = vec![decl];
    let mut seen: HashSet<DeclarationId> = HashSet::new();
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        if memoized.contains(&current) {
            return true;
        }
        if let Some(nexts) = definitions.get(&current) {
            for &next in nexts {
                if !seen.contains(&next) {
                    stack.push(next);
                }
            }
        }
    }
    false
}

fn scope_feeds_flattened_conditional_dependency(
    block: &ReactiveBlock,
    start_index: usize,
    scope_decl_ids: &HashSet<DeclarationId>,
) -> bool {
    let mut conditional_results: HashSet<DeclarationId> = HashSet::new();

    for stmt in block.iter().skip(start_index) {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                let result_decl = instr.lvalue.as_ref().map(|lv| lv.identifier.declaration_id);
                let uses_scope_decl = match &instr.value {
                    InstructionValue::Ternary {
                        consequent,
                        alternate,
                        ..
                    } => {
                        scope_decl_ids.contains(&consequent.identifier.declaration_id)
                            || scope_decl_ids.contains(&alternate.identifier.declaration_id)
                    }
                    InstructionValue::LogicalExpression { left, right, .. } => {
                        scope_decl_ids.contains(&left.identifier.declaration_id)
                            || scope_decl_ids.contains(&right.identifier.declaration_id)
                    }
                    _ => false,
                };
                if uses_scope_decl && let Some(decl) = result_decl {
                    conditional_results.insert(decl);
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                if scope_depends_on_any(scope_block, &conditional_results) {
                    return true;
                }
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                if scope_depends_on_any_pruned(scope_block, &conditional_results) {
                    return true;
                }
            }
            ReactiveStatement::Terminal(_) => {}
        }
    }

    false
}

fn scope_depends_on_any(scope_block: &ReactiveScopeBlock, deps: &HashSet<DeclarationId>) -> bool {
    scope_block
        .scope
        .dependencies
        .iter()
        .any(|dep| deps.contains(&dep.identifier.declaration_id))
}

fn scope_depends_on_any_pruned(
    scope_block: &PrunedReactiveScopeBlock,
    deps: &HashSet<DeclarationId>,
) -> bool {
    scope_block
        .scope
        .dependencies
        .iter()
        .any(|dep| deps.contains(&dep.identifier.declaration_id))
}

fn debug_print_scopes(block: &ReactiveBlock, memoized: &HashSet<DeclarationId>) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Scope(scope_block) => {
                let mut decls: Vec<u32> = scope_block
                    .scope
                    .declarations
                    .values()
                    .map(|d| d.identifier.declaration_id.0)
                    .collect();
                decls.sort_unstable();
                let mut mem: Vec<u32> = scope_block
                    .scope
                    .declarations
                    .values()
                    .map(|d| d.identifier.declaration_id.0)
                    .filter(|id| memoized.contains(&DeclarationId(*id)))
                    .collect();
                mem.sort_unstable();
                eprintln!(
                    "[PRUNE_HOOK] scope={} decls={:?} memoized_decls={:?}",
                    scope_block.scope.id.0, decls, mem
                );
                debug_print_scopes(&scope_block.instructions, memoized);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                debug_print_scopes(&scope_block.instructions, memoized);
            }
            ReactiveStatement::Terminal(term_stmt) => match &term_stmt.terminal {
                ReactiveTerminal::If {
                    consequent,
                    alternate,
                    ..
                } => {
                    debug_print_scopes(consequent, memoized);
                    if let Some(alt) = alternate {
                        debug_print_scopes(alt, memoized);
                    }
                }
                ReactiveTerminal::Switch { cases, .. } => {
                    for case in cases {
                        if let Some(b) = &case.block {
                            debug_print_scopes(b, memoized);
                        }
                    }
                }
                ReactiveTerminal::DoWhile { loop_block, .. }
                | ReactiveTerminal::While { loop_block, .. } => {
                    debug_print_scopes(loop_block, memoized);
                }
                ReactiveTerminal::For {
                    init,
                    update,
                    loop_block,
                    ..
                } => {
                    debug_print_scopes(init, memoized);
                    if let Some(upd) = update {
                        debug_print_scopes(upd, memoized);
                    }
                    debug_print_scopes(loop_block, memoized);
                }
                ReactiveTerminal::ForOf {
                    init, loop_block, ..
                }
                | ReactiveTerminal::ForIn {
                    init, loop_block, ..
                } => {
                    debug_print_scopes(init, memoized);
                    debug_print_scopes(loop_block, memoized);
                }
                ReactiveTerminal::Label { block, .. } => {
                    debug_print_scopes(block, memoized);
                }
                ReactiveTerminal::Try { block, handler, .. } => {
                    debug_print_scopes(block, memoized);
                    debug_print_scopes(handler, memoized);
                }
                ReactiveTerminal::Break { .. }
                | ReactiveTerminal::Continue { .. }
                | ReactiveTerminal::Return { .. }
                | ReactiveTerminal::Throw { .. } => {}
            },
            ReactiveStatement::Instruction(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Prunes reactive scopes whose outputs do not escape. This is the main
/// entry point, mirroring upstream `pruneNonEscapingScopes`.
///
/// The `force_memoize_primitives` flag controls whether primitive-producing
/// instructions are considered for memoization. When true (corresponding to
/// upstream `enablePreserveExistingMemoizationGuarantees` or `enableForest`),
/// primitives are treated as Conditional rather than Never.
pub fn prune_non_escaping_scopes(func: &mut ReactiveFunction) {
    prune_non_escaping_scopes_with_options(func, false);
}

/// Same as `prune_non_escaping_scopes` but allows configuring
/// `force_memoize_primitives`.
pub fn prune_non_escaping_scopes_with_options(
    func: &mut ReactiveFunction,
    force_memoize_primitives: bool,
) {
    let options = MemoizationOptions {
        // Default: always memoize JSX elements (upstream enableForest = false)
        memoize_jsx_elements: true,
        force_memoize_primitives,
    };

    // Phase 1: Build the dependency graph
    let mut state = State::new();
    let (id_to_name, load_source) = build_name_and_load_lookups(func);
    let conditional_only_decls = collect_conditional_only_declarations(func);
    let conditional_fallback_decls = collect_conditional_fallback_declarations(func);
    if std::env::var("DEBUG_PRUNE_HOOKS").is_ok() {
        let mut conditional_only: Vec<u32> =
            conditional_only_decls.iter().map(|decl| decl.0).collect();
        conditional_only.sort_unstable();
        eprintln!("[PRUNE_HOOK] conditional_only_decls={:?}", conditional_only);
        let mut conditional_fallback: Vec<u32> = conditional_fallback_decls
            .iter()
            .map(|decl| decl.0)
            .collect();
        conditional_fallback.sort_unstable();
        eprintln!(
            "[PRUNE_HOOK] conditional_fallback_decls={:?}",
            conditional_fallback
        );
    }

    // Declare params
    for param in &func.params {
        match param {
            Argument::Place(p) => state.declare(p.identifier.declaration_id),
            Argument::Spread(p) => state.declare(p.identifier.declaration_id),
        }
    }

    collect_dependencies_block(
        &mut state,
        &func.body,
        &[],
        &options,
        &conditional_only_decls,
        &conditional_fallback_decls,
        &id_to_name,
        &load_source,
    );

    // Phase 2: Walk from escaping values to determine memoized set
    let memoized = compute_memoized_identifiers(&mut state);

    if std::env::var("DEBUG_PRUNE_HOOKS").is_ok() {
        let mut escaping: Vec<u32> = state.escaping_values.iter().map(|d| d.0).collect();
        escaping.sort_unstable();
        let mut memoized_ids: Vec<u32> = memoized.iter().map(|d| d.0).collect();
        memoized_ids.sort_unstable();
        eprintln!("[PRUNE_HOOK] escaping_values={:?}", escaping);
        eprintln!("[PRUNE_HOOK] memoized_values={:?}", memoized_ids);
        let mut ids: Vec<DeclarationId> = state.identifiers.keys().copied().collect();
        ids.sort_unstable_by_key(|id| id.0);
        for id in ids {
            if let Some(node) = state.identifiers.get(&id) {
                let mut deps: Vec<u32> = node.dependencies.iter().map(|d| d.0).collect();
                deps.sort_unstable();
                let mut scopes: Vec<u32> = node.scopes.iter().map(|s| s.0).collect();
                scopes.sort_unstable();
                eprintln!(
                    "[PRUNE_HOOK_NODE] id={} level={:?} memoized={} seen={} deps={:?} scopes={:?}",
                    id.0, node.level, node.memoized, node.seen, deps, scopes
                );
            }
        }
        debug_print_scopes(&func.body, &memoized);
    }

    // Phase 3: Prune scopes that do not declare/reassign any escaping values
    let mut pruned_scopes = HashSet::new();
    let mut reassignments: HashMap<
        DeclarationId,
        HashSet<(IdentifierId, DeclarationId, Option<ScopeId>)>,
    > = HashMap::new();
    prune_scopes_block(
        &mut func.body,
        &memoized,
        &state.definitions,
        &mut pruned_scopes,
        &mut reassignments,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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

    fn make_identifier_with_scope(
        id: u32,
        name: Option<&str>,
        scope_id: u32,
        range_start: u32,
        range_end: u32,
    ) -> Identifier {
        Identifier {
            id: IdentifierId(id),
            declaration_id: DeclarationId(id),
            name: name.map(|n| IdentifierName::Named(n.to_string())),
            mutable_range: MutableRange::default(),
            scope: Some(Box::new(ReactiveScope {
                id: ScopeId(scope_id),
                range: MutableRange {
                    start: InstructionId(range_start),
                    end: InstructionId(range_end),
                },
                dependencies: vec![],
                declarations: Default::default(),
                reassignments: vec![],
                merged_id: None,
                early_return_value: None,
            })),
            type_: Type::Poly,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place(id: u32, name: Option<&str>) -> Place {
        Place {
            identifier: make_identifier(id, name),
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_place_with_scope(
        id: u32,
        name: Option<&str>,
        scope_id: u32,
        range_start: u32,
        range_end: u32,
    ) -> Place {
        Place {
            identifier: make_identifier_with_scope(id, name, scope_id, range_start, range_end),
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    fn make_reactive_scope(scope_id: u32, decl_ids: &[(u32, Option<&str>)]) -> ReactiveScope {
        let mut declarations = HashMap::new();
        for &(id, name) in decl_ids {
            declarations.insert(
                IdentifierId(id),
                ScopeDeclaration {
                    identifier: make_identifier(id, name),
                    scope: make_declaration_scope(ScopeId(scope_id)),
                },
            );
        }
        ReactiveScope {
            id: ScopeId(scope_id),
            range: MutableRange {
                start: InstructionId(0),
                end: InstructionId(100),
            },
            dependencies: vec![],
            declarations,
            reassignments: vec![],
            merged_id: None,
            early_return_value: None,
        }
    }

    #[test]
    fn test_join_aliases() {
        assert_eq!(
            join_aliases(MemoizationLevel::Memoized, MemoizationLevel::Never),
            MemoizationLevel::Memoized
        );
        assert_eq!(
            join_aliases(MemoizationLevel::Conditional, MemoizationLevel::Never),
            MemoizationLevel::Conditional
        );
        assert_eq!(
            join_aliases(MemoizationLevel::Never, MemoizationLevel::Never),
            MemoizationLevel::Never
        );
        assert_eq!(
            join_aliases(MemoizationLevel::Unmemoized, MemoizationLevel::Never),
            MemoizationLevel::Unmemoized
        );
    }

    #[test]
    fn test_is_mutable_effect() {
        assert!(is_mutable_effect(Effect::Mutate));
        assert!(is_mutable_effect(Effect::Capture));
        assert!(is_mutable_effect(Effect::Store));
        assert!(is_mutable_effect(Effect::ConditionallyMutate));
        assert!(!is_mutable_effect(Effect::Read));
        assert!(!is_mutable_effect(Effect::Freeze));
    }

    #[test]
    fn test_prune_non_escaping_simple() {
        // Simple case: an object expression that is not returned should be pruned
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_reactive_scope(0, &[(1, Some("a"))]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place_with_scope(1, Some("a"), 0, 0, 100)),
                            value: InstructionValue::ObjectExpression {
                                properties: vec![],
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: make_place(99, None),
                        id: InstructionId(10),
                        loc: SourceLocation::Generated,
                    },
                    label: None,
                }),
            ],
            directives: vec![],
        };

        prune_non_escaping_scopes(&mut func);

        // The scope should be pruned since `a` doesn't escape
        assert!(
            !matches!(&func.body[0], ReactiveStatement::Scope(_)),
            "Scope should have been pruned since its output does not escape"
        );
    }

    #[test]
    fn test_keep_escaping_scope() {
        // The object is returned, so its scope should be kept
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_reactive_scope(0, &[(1, Some("a"))]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place_with_scope(1, Some("a"), 0, 0, 100)),
                            value: InstructionValue::ObjectExpression {
                                properties: vec![],
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: make_place(1, Some("a")),
                        id: InstructionId(10),
                        loc: SourceLocation::Generated,
                    },
                    label: None,
                }),
            ],
            directives: vec![],
        };

        prune_non_escaping_scopes(&mut func);

        // The scope should be kept since `a` is returned
        assert!(
            matches!(&func.body[0], ReactiveStatement::Scope(_)),
            "Scope should be kept since its output escapes via return"
        );
    }

    #[test]
    fn test_empty_scope_kept() {
        // Empty scopes (no declarations or reassignments) should be kept
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![ReactiveStatement::Scope(ReactiveScopeBlock {
                scope: ReactiveScope {
                    id: ScopeId(0),
                    range: MutableRange {
                        start: InstructionId(0),
                        end: InstructionId(10),
                    },
                    dependencies: vec![],
                    declarations: Default::default(),
                    reassignments: vec![],
                    merged_id: None,
                    early_return_value: None,
                },
                instructions: vec![],
            })],
            directives: vec![],
        };

        prune_non_escaping_scopes(&mut func);

        // Empty scope should be kept (pruned later by PruneUnusedScopes)
        assert!(matches!(&func.body[0], ReactiveStatement::Scope(_)));
    }

    #[test]
    fn test_hook_args_escape() {
        // Values passed to hooks should be treated as escaping
        let mut func = ReactiveFunction {
            loc: SourceLocation::Generated,
            id: None,
            name_hint: None,
            params: vec![],
            generator: false,
            async_: false,
            body: vec![
                ReactiveStatement::Scope(ReactiveScopeBlock {
                    scope: make_reactive_scope(0, &[(1, Some("callback"))]),
                    instructions: vec![ReactiveStatement::Instruction(Box::new(
                        ReactiveInstruction {
                            id: InstructionId(1),
                            lvalue: Some(make_place_with_scope(1, Some("callback"), 0, 0, 100)),
                            value: InstructionValue::FunctionExpression {
                                name: None,
                                lowered_func: LoweredFunction {
                                    func: HIRFunction {
                                        env: crate::environment::Environment::new(
                                            crate::options::EnvironmentConfig::default(),
                                        ),
                                        loc: SourceLocation::Generated,
                                        id: None,
                                        fn_type: ReactFunctionType::Other,
                                        params: vec![],
                                        returns: make_place(99, None),
                                        context: vec![],
                                        body: HIR {
                                            entry: BlockId(0),
                                            blocks: vec![],
                                        },
                                        generator: false,
                                        async_: false,
                                        directives: vec![],
                                        aliasing_effects: None,
                                    },
                                },
                                expr_type: FunctionExpressionType::ArrowFunctionExpression,
                                loc: SourceLocation::Generated,
                            },
                            loc: SourceLocation::Generated,
                        },
                    ))],
                }),
                // useEffect(callback)
                ReactiveStatement::Instruction(Box::new(ReactiveInstruction {
                    id: InstructionId(5),
                    lvalue: Some(make_place(10, None)),
                    value: InstructionValue::CallExpression {
                        callee: make_place(50, Some("useEffect")),
                        args: vec![Argument::Place(make_place(1, Some("callback")))],
                        optional: false,
                        loc: SourceLocation::Generated,
                    },
                    loc: SourceLocation::Generated,
                })),
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Return {
                        value: make_place(99, None),
                        id: InstructionId(20),
                        loc: SourceLocation::Generated,
                    },
                    label: None,
                }),
            ],
            directives: vec![],
        };

        prune_non_escaping_scopes(&mut func);

        // The scope for `callback` should be kept because it's passed to a hook
        assert!(
            matches!(&func.body[0], ReactiveStatement::Scope(_)),
            "Scope should be kept since callback is passed to useEffect"
        );
    }
}
