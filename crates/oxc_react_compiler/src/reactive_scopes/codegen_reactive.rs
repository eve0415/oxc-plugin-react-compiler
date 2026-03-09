//! Codegen — generate JavaScript from a ReactiveFunction tree.
//!
//! Port of `CodegenReactiveFunction.ts` from upstream babel-plugin-react-compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! This walks the tree-shaped ReactiveFunction (produced by buildReactiveFunction)
//! and emits JavaScript code with memoization cache guards.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::{SPAN, SourceType};
use oxc_syntax::number::NumberBase;
use oxc_syntax::operator::{
    AssignmentOperator, BinaryOperator as AstBinaryOperator,
    LogicalOperator as AstLogicalOperator, UnaryOperator as AstUnaryOperator,
    UpdateOperator as AstUpdateOperator,
};

use crate::environment::Environment;
use crate::error::{BailOut, CompilerDiagnostic, CompilerError, DiagnosticSeverity};
use crate::hir::object_shape::BUILT_IN_ARRAY_ID;
use crate::hir::types::*;
use crate::hir::visitors;

pub const MEMO_CACHE_SENTINEL: &str = "react.memo_cache_sentinel";
pub const EARLY_RETURN_SENTINEL: &str = "react.early_return_sentinel";
pub const HOOK_GUARD_IDENT: &str = "$dispatcherGuard";
pub(crate) const HOOK_GUARD_PUSH: u8 = 0;
pub(crate) const HOOK_GUARD_POP: u8 = 1;
const HOOK_GUARD_ALLOW: u8 = 2;
const HOOK_GUARD_DISALLOW: u8 = 3;

thread_local! {
    static FAST_REFRESH_SOURCE_HASH: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn set_fast_refresh_source_hash(hash: Option<String>) {
    FAST_REFRESH_SOURCE_HASH.with(|slot| {
        *slot.borrow_mut() = hash;
    });
}

fn get_fast_refresh_source_hash() -> Option<String> {
    FAST_REFRESH_SOURCE_HASH.with(|slot| slot.borrow().clone())
}

/// Expression precedence levels (matching JavaScript operator precedence).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum ExprPrecedence {
    /// Assignment: a = b, a += b, etc.
    Assignment = 3,
    /// Conditional/ternary: a ? b : c
    Conditional = 4,
    /// Nullish coalescing: a ?? b
    NullishCoalescing = 5,
    /// Logical OR: a || b
    LogicalOr = 6,
    /// Logical AND: a && b
    LogicalAnd = 7,
    /// Bitwise OR: a | b
    BitwiseOr = 8,
    /// Bitwise XOR: a ^ b
    BitwiseXor = 9,
    /// Bitwise AND: a & b
    BitwiseAnd = 10,
    /// Equality: a == b, a === b, a != b, a !== b
    Equality = 11,
    /// Relational: a < b, a > b, a <= b, a >= b, a instanceof b, a in b
    Relational = 12,
    /// Shift: a << b, a >> b, a >>> b
    Shift = 13,
    /// Additive: a + b, a - b
    Additive = 14,
    /// Multiplicative: a * b, a / b, a % b
    Multiplicative = 15,
    /// Exponentiation: a ** b
    Exponentiation = 16,
    /// Unary: !a, -a, +a, ~a, typeof, void, delete
    Unary = 17,
    /// Primary/atomic: identifiers, literals, member access, calls, arrays, objects, parens
    Primary = 20,
}

/// Kind of expression value, used to distinguish JSXText from regular expressions.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ExprKind {
    Normal,
    JsxText,
}

/// An expression value with its precedence level and kind.
#[derive(Clone, Debug)]
struct ExprValue {
    expr: String,
    prec: ExprPrecedence,
    kind: ExprKind,
}

impl ExprValue {
    fn new(expr: String, prec: ExprPrecedence) -> Self {
        Self {
            expr,
            prec,
            kind: ExprKind::Normal,
        }
    }

    fn primary(expr: String) -> Self {
        Self {
            expr,
            prec: ExprPrecedence::Primary,
            kind: ExprKind::Normal,
        }
    }

    fn jsx_text(expr: String) -> Self {
        Self {
            expr,
            prec: ExprPrecedence::Primary,
            kind: ExprKind::JsxText,
        }
    }

    /// Wrap in parentheses if our precedence is lower than the required minimum.
    fn wrap_if_needed(&self, min_prec: ExprPrecedence) -> String {
        if self.prec < min_prec {
            format!("({})", self.expr)
        } else {
            self.expr.clone()
        }
    }
}

/// Result of codegen for a reactive function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastRefreshPrologue {
    pub cache_index: u32,
    pub hash: String,
    pub index_binding_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePrologue {
    pub binding_name: String,
    pub size: u32,
    pub fast_refresh: Option<FastRefreshPrologue>,
}

pub struct CodegenResult {
    /// Generated function body (statements inside the function).
    pub body: String,
    /// Number of cache slots used.
    pub cache_size: u32,
    /// Whether the function needs the cache import.
    pub needs_cache_import: bool,
    /// Rendered parameter names for this function body.
    pub param_names: Vec<String>,
    /// Whether the function needs the makeReadOnly import (enableEmitFreeze).
    pub needs_freeze_import: bool,
    /// Whether this function has a fire rewrite (needs useFire import).
    pub has_fire_rewrite: bool,
    /// Whether this function emitted runtime hook guards.
    pub needs_hook_guards: bool,
    /// Whether this function needs the top-level hook guard try/finally wrapper.
    pub needs_function_hook_guard_wrapper: bool,
    /// Whether this function emitted `$structuralCheck` calls.
    pub needs_structural_check_import: bool,
    /// Structured cache prologue metadata for AST emission.
    pub cache_prologue: Option<CachePrologue>,
    /// Deferred codegen error (upstream invariant parity).
    pub error: Option<CompilerError>,
}

/// Codegen context tracking temporaries, declarations, and cache slots.
#[derive(Clone)]
struct Context {
    /// Next cache slot index.
    next_cache_index: u32,
    /// Set of declared identifiers (by DeclarationId) to dedupe declarations.
    declarations: HashSet<DeclarationId>,
    /// Declarations whose runtime `let/const/function` binding has been emitted.
    runtime_emitted_declarations: HashSet<DeclarationId>,
    /// Temporary values keyed by declaration identity (upstream parity).
    /// This allows different IdentifierIds of the same declaration to reuse
    /// the same materialized expression.
    temp: HashMap<DeclarationId, Option<ExprValue>>,
    /// Branch-local unnamed temps need exact SSA identity to avoid collapsing
    /// distinct value-block arms that intentionally share a declaration id.
    temp_by_identifier: HashMap<IdentifierId, Option<ExprValue>>,
    /// Object method values: maps IdentifierId -> index into object_methods_store.
    object_methods: HashMap<IdentifierId, usize>,
    /// Storage for ObjectMethod instruction values.
    object_methods_store: Vec<ObjectMethodInfo>,
    /// Callback dependency expressions keyed by declaration id.
    /// Used for replacing AUTODEPS with inferred dependency arrays.
    callback_deps: HashMap<DeclarationId, Vec<String>>,
    /// Declaration IDs that are passed as arguments to hook calls.
    hook_callback_arg_decls: HashSet<DeclarationId>,
    /// Best-effort resolved identifier names by identifier id.
    /// Used for hook/stable-ref detection through lowered aliases.
    resolved_names: HashMap<IdentifierId, String>,
    /// Unnamed temp ids fused away from emitted output. We shift later temp
    /// names down so generated names align with upstream snapshots.
    suppressed_temp_ids: Vec<u32>,
    /// Hook callee names keyed by declaration id of the call result.
    hook_call_by_decl: HashMap<DeclarationId, String>,
    /// Declaration IDs known to hold stable `useRef()` return values (or aliases).
    stable_ref_decls: HashSet<DeclarationId>,
    /// Declaration IDs for stable setter/dispatch values.
    stable_setter_decls: HashSet<DeclarationId>,
    /// Declaration IDs for non-reactive effect-event functions.
    stable_effect_event_decls: HashSet<DeclarationId>,
    /// Declaration IDs assigned from multiple source declarations via reassign.
    multi_source_decls: HashSet<DeclarationId>,
    /// Track source declarations assigned into each declaration (for multi-source detection).
    decl_assignment_sources: HashMap<DeclarationId, HashSet<DeclarationId>>,
    /// Stable-ref names for dependency string post-processing.
    stable_ref_names: HashSet<String>,
    /// Set of all unique identifier names in the function (for synthesizing conflict-free names).
    unique_identifiers: HashSet<String>,
    /// Identifier ids that are operands of fbt/fbs macros.
    /// For these values we must preserve JSX string-attribute form (no forced expr container).
    fbt_operands: HashSet<IdentifierId>,
    /// Synthesized names cache.
    synthesized_names: HashMap<String, String>,
    /// Next temp variable index for renumbering unnamed temporaries.
    next_temp_index: u32,
    /// Maps IdentifierId -> renumbered temp index for unnamed identifiers.
    temp_remap: HashMap<IdentifierId, u32>,
    /// Names already declared in emitted output.
    declared_names: HashSet<String>,
    /// Declaration ids captured by nested lowered functions as context values.
    captured_in_child_functions: HashSet<DeclarationId>,
    /// Subset of captured child-context declarations that are mutable/captured
    /// (effectful) in child functions.
    mutable_captured_in_child_functions: HashSet<DeclarationId>,
    /// Declaration ids reassigned at least once in this reactive function.
    reassigned_decls: HashSet<DeclarationId>,
    /// Declaration ids that are read somewhere in the reactive function body.
    read_declarations: HashSet<DeclarationId>,
    /// Primitive literals inherited from parent lexical scopes and safe to inline.
    inline_primitive_literals: HashMap<DeclarationId, String>,
    /// Primitive literals discovered in this function and safe to pass to child scopes.
    capturable_primitive_literals: HashMap<DeclarationId, String>,
    /// Declaration IDs that originate from non-local/global bindings.
    non_local_binding_decls: HashSet<DeclarationId>,
    /// Disable memoization-specific codegen paths (retry pipeline).
    disable_memoization_features: bool,
    disable_memoization_for_debugging: bool,
    /// Emit explicit per-slot change variables (`c_N`) for memo guards.
    enable_change_variable_codegen: bool,
    /// Emit runtime hook guards around inferred-memoized hook usage.
    emit_hook_guards: bool,
    /// Emit change-detection debug code (`$structuralCheck`) for memo scopes.
    enable_change_detection_for_debugging: bool,
    /// Wrap anonymous function expressions with generated name hints.
    enable_name_anonymous_functions: bool,
    /// Whether `$structuralCheck` was emitted by this function.
    needs_structural_check_import: bool,
    /// Function display name used by debug change-detection diagnostics.
    function_name: String,
    /// Canonical display names for function parameters keyed by declaration id.
    param_display_names: HashMap<DeclarationId, String>,
    /// Names declared in nested lowered functions; used to avoid reusing those
    /// names for subsequent declarations in the outer function.
    reserved_child_decl_names: HashSet<String>,
    /// Temp-like runtime bindings declared in currently active lexical blocks.
    block_scope_declared_temp_names: Vec<HashSet<String>>,
    /// Stable emitted names by declaration id.
    declaration_name_overrides: HashMap<DeclarationId, String>,
    /// Emitted declaration names used in this function body.
    used_declaration_names: HashSet<String>,
    /// Preferred display names discovered from named identifiers in the
    /// reactive function body, keyed by declaration id.
    preferred_decl_names: HashMap<DeclarationId, String>,
    /// First invariant/codegen error encountered while emitting this function.
    codegen_error: Option<CompilerError>,
    /// Optional dependency reads already emitted as standalone pre-read statements.
    emitted_optional_dep_reads: HashSet<String>,
    /// Named local declarations that should emit a standalone dependency read
    /// when bridged through a temporary after StartMemoize.
    pending_manual_memo_reads: HashSet<DeclarationId>,
    /// Named local declarations referenced directly by StartMemoize roots.
    /// Upstream currently preserves these bindings even when constant
    /// propagation removes the runtime initializer.
    manual_memo_root_decls: HashSet<DeclarationId>,
    /// Manual memo dependency roots keyed by manual memo marker id.
    manual_memo_dep_roots_by_id: HashMap<u32, HashSet<DeclarationId>>,
    /// Manual memo dependency roots keyed by memoized declaration id.
    manual_memo_dep_roots_by_decl: HashMap<DeclarationId, HashSet<DeclarationId>>,
    /// Declaration IDs produced by pruned FinishMemoize markers.
    pruned_manual_memo_decls: HashSet<DeclarationId>,
    /// Declarations produced by dependency-free scopes. These values are
    /// sentinel-initialized once and then stable across renders.
    stable_zero_dep_decls: HashSet<DeclarationId>,
    /// Declaration IDs referenced by any reactive scope dependency.
    scope_dependency_decls: HashSet<DeclarationId>,
    /// Dependency overrides for scopes whose temporary dependency should be
    /// expanded back to source reactive inputs during codegen.
    scope_dependency_overrides: HashMap<ScopeId, Vec<ReactiveScopeDependency>>,
    /// Declaration IDs that originate from function declarations.
    function_decl_decls: HashSet<DeclarationId>,
    /// Declarations that are read only as JSX component tags and nowhere else.
    jsx_only_component_tag_decls: HashSet<DeclarationId>,
    /// Inline aliases for declarations that can be emitted without runtime bindings.
    inline_identifier_aliases: HashMap<DeclarationId, String>,
    /// Declarations whose runtime binding emission was intentionally elided.
    elided_named_declarations: HashSet<DeclarationId>,
    /// Preserve literal const/let initializers in loop headers (don't DCE them).
    preserve_loop_header_inits: bool,
    /// `let` bindings emitted for scope outputs in each active lexical block.
    block_scope_output_names: Vec<HashSet<String>>,
}

type TempSnapshot = (
    HashMap<DeclarationId, Option<ExprValue>>,
    HashMap<IdentifierId, Option<ExprValue>>,
);

#[derive(Clone)]
struct ObjectMethodInfo {
    lowered_func: LoweredFunction,
}

#[derive(Clone, Copy)]
pub struct CodegenReactiveOptions {
    pub disable_memoization_features: bool,
    pub disable_memoization_for_debugging: bool,
    pub enable_change_variable_codegen: bool,
    pub enable_emit_hook_guards: bool,
    pub enable_change_detection_for_debugging: bool,
    pub enable_reset_cache_on_source_file_changes: bool,
    pub enable_name_anonymous_functions: bool,
    pub emit_directives_in_body: bool,
    pub emit_function_hook_guard_wrapper_in_body: bool,
}

impl Default for CodegenReactiveOptions {
    fn default() -> Self {
        Self {
            disable_memoization_features: false,
            disable_memoization_for_debugging: false,
            enable_change_variable_codegen: false,
            enable_emit_hook_guards: false,
            enable_change_detection_for_debugging: false,
            enable_reset_cache_on_source_file_changes: false,
            enable_name_anonymous_functions: false,
            emit_directives_in_body: true,
            emit_function_hook_guard_wrapper_in_body: true,
        }
    }
}

#[derive(Default)]
struct CodegenReactiveInputs {
    inline_primitive_literals: HashMap<DeclarationId, String>,
    inherited_declaration_name_overrides: HashMap<DeclarationId, String>,
    initial_temp_snapshot: TempSnapshot,
    fbt_operands: HashSet<IdentifierId>,
    inherited_reserved_child_decl_names: HashSet<String>,
    emit_function_hook_guard: bool,
}

impl Context {
    fn alloc_cache_slot(&mut self) -> u32 {
        let idx = self.next_cache_index;
        self.next_cache_index += 1;
        idx
    }

    fn declare(&mut self, id: &Identifier) {
        self.declarations.insert(id.declaration_id);
    }

    fn has_declared(&self, id: &Identifier) -> bool {
        self.declarations.contains(&id.declaration_id)
    }

    fn set_temp_expr(&mut self, id: &Identifier, value: Option<ExprValue>) {
        self.temp.insert(id.declaration_id, value.clone());
        if is_temp_like_identifier(self, id) {
            self.temp_by_identifier.insert(id.id, value);
        }
    }

    fn temp_expr_for_place(&self, place: &Place) -> Option<Option<ExprValue>> {
        if is_temp_like_identifier(self, &place.identifier)
            && let Some(value) = self.temp_by_identifier.get(&place.identifier.id)
        {
            return Some(value.clone());
        }
        self.temp.get(&place.identifier.declaration_id).cloned()
    }

    fn snapshot_temps(&self) -> TempSnapshot {
        (self.temp.clone(), self.temp_by_identifier.clone())
    }

    fn restore_temps(&mut self, snapshot: TempSnapshot) {
        let (temp, temp_by_identifier) = snapshot;
        self.temp = temp;
        self.temp_by_identifier = temp_by_identifier;
    }

    fn mark_decl_runtime_emitted(&mut self, decl_id: DeclarationId) {
        if let Ok(filter) = std::env::var("DEBUG_DECL_EMIT")
            && filter
                .split(',')
                .filter_map(|part| part.trim().parse::<u32>().ok())
                .any(|id| id == decl_id.0)
        {
            eprintln!(
                "[DECL_RUNTIME_EMIT] decl={} backtrace=\n{}",
                decl_id.0,
                std::backtrace::Backtrace::force_capture()
            );
        }
        self.runtime_emitted_declarations.insert(decl_id);
    }

    fn has_declared_by_runtime_emission(&self, decl_id: DeclarationId) -> bool {
        self.runtime_emitted_declarations.contains(&decl_id)
    }

    fn synthesize_name(&mut self, name: &str) -> String {
        if let Some(prev) = self.synthesized_names.get(name) {
            return prev.clone();
        }
        let mut validated = name.to_string();
        let mut index = 0u32;
        while self.unique_identifiers.contains(&validated) {
            validated = format!("{}{}", name, index);
            index += 1;
        }
        self.unique_identifiers.insert(validated.clone());
        self.synthesized_names
            .insert(name.to_string(), validated.clone());
        validated
    }

    fn mark_stable_ref_identifier(&mut self, id: &Identifier) {
        self.stable_ref_decls.insert(id.declaration_id);
        if let Some(name) = &id.name {
            self.stable_ref_names.insert(name.value().to_string());
        }
    }

    fn mark_stable_setter_identifier(&mut self, id: &Identifier) {
        self.stable_setter_decls.insert(id.declaration_id);
    }

    fn primitive_literals_for_child(&self) -> HashMap<DeclarationId, String> {
        let mut merged = self.inline_primitive_literals.clone();
        for (decl, value) in &self.capturable_primitive_literals {
            merged.insert(*decl, value.clone());
        }
        merged
    }

    fn child_codegen_options(&self) -> CodegenReactiveOptions {
        CodegenReactiveOptions {
            disable_memoization_features: self.disable_memoization_features,
            disable_memoization_for_debugging: self.disable_memoization_for_debugging,
            enable_change_variable_codegen: self.enable_change_variable_codegen,
            enable_emit_hook_guards: self.emit_hook_guards,
            enable_change_detection_for_debugging: self.enable_change_detection_for_debugging,
            enable_reset_cache_on_source_file_changes: false,
            enable_name_anonymous_functions: self.enable_name_anonymous_functions,
            emit_directives_in_body: true,
            emit_function_hook_guard_wrapper_in_body: true,
        }
    }
}

/// Generate code from a ReactiveFunction tree.
pub fn codegen_reactive_function(
    func: &ReactiveFunction,
    unique_identifiers: HashSet<String>,
) -> CodegenResult {
    codegen_reactive_function_with_options_and_fbt_operands(
        func,
        unique_identifiers,
        CodegenReactiveOptions::default(),
        HashSet::new(),
    )
}

pub fn codegen_reactive_function_with_options(
    func: &ReactiveFunction,
    unique_identifiers: HashSet<String>,
    disable_memoization_features: bool,
    enable_change_variable_codegen: bool,
    enable_emit_hook_guards: bool,
    enable_change_detection_for_debugging: bool,
) -> CodegenResult {
    codegen_reactive_function_with_options_and_fbt_operands(
        func,
        unique_identifiers,
        CodegenReactiveOptions {
            disable_memoization_features,
            enable_change_variable_codegen,
            enable_emit_hook_guards,
            enable_change_detection_for_debugging,
            ..CodegenReactiveOptions::default()
        },
        HashSet::new(),
    )
}

pub fn codegen_reactive_function_with_options_and_fbt_operands(
    func: &ReactiveFunction,
    unique_identifiers: HashSet<String>,
    options: CodegenReactiveOptions,
    fbt_operands: HashSet<IdentifierId>,
) -> CodegenResult {
    codegen_reactive_function_with_primitives(
        func,
        unique_identifiers,
        CodegenReactiveInputs {
            emit_function_hook_guard: options.enable_emit_hook_guards,
            fbt_operands,
            ..CodegenReactiveInputs::default()
        },
        options,
    )
}

fn codegen_reactive_function_with_primitives(
    func: &ReactiveFunction,
    unique_identifiers: HashSet<String>,
    inputs: CodegenReactiveInputs,
    options: CodegenReactiveOptions,
) -> CodegenResult {
    let CodegenReactiveInputs {
        inline_primitive_literals,
        inherited_declaration_name_overrides,
        initial_temp_snapshot,
        fbt_operands,
        inherited_reserved_child_decl_names,
        emit_function_hook_guard,
    } = inputs;
    let captured_in_child_functions = collect_child_context_declarations(&func.body);
    let mutable_captured_in_child_functions =
        collect_mutable_child_context_declarations(&func.body);
    let mut reassigned_decls = collect_reassigned_declarations(&func.body);
    reassigned_decls.extend(mutable_captured_in_child_functions.iter().copied());
    let read_declarations = collect_read_declarations(&func.body);
    let manual_memo_root_decls = collect_manual_memo_root_declarations(&func.body);
    let function_decl_decls = collect_function_declarations(&func.body);
    let jsx_only_component_tag_decls = collect_jsx_only_component_tag_declarations(&func.body);
    let scope_dependency_decls = collect_scope_dependency_declarations(&func.body);
    let inferred_memo_enabled = !func
        .directives
        .iter()
        .any(|d| d == "use memo" || d == "use forget" || d.starts_with("use memo if("));
    let emit_hook_guards = options.enable_emit_hook_guards
        && inferred_memo_enabled
        && !options.disable_memoization_features;
    let function_name = func
        .name_hint
        .clone()
        .or_else(|| func.id.clone())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let (temp, temp_by_identifier) = initial_temp_snapshot;
    let mut cx = Context {
        next_cache_index: 0,
        declarations: HashSet::new(),
        runtime_emitted_declarations: HashSet::new(),
        temp,
        temp_by_identifier,
        object_methods: HashMap::new(),
        object_methods_store: Vec::new(),
        callback_deps: HashMap::new(),
        hook_callback_arg_decls: HashSet::new(),
        resolved_names: HashMap::new(),
        suppressed_temp_ids: Vec::new(),
        hook_call_by_decl: HashMap::new(),
        stable_ref_decls: HashSet::new(),
        stable_setter_decls: HashSet::new(),
        stable_effect_event_decls: HashSet::new(),
        multi_source_decls: HashSet::new(),
        decl_assignment_sources: HashMap::new(),
        stable_ref_names: HashSet::new(),
        unique_identifiers,
        fbt_operands,
        synthesized_names: HashMap::new(),
        next_temp_index: 0,
        temp_remap: HashMap::new(),
        declared_names: HashSet::new(),
        captured_in_child_functions,
        mutable_captured_in_child_functions,
        reassigned_decls,
        read_declarations,
        inline_primitive_literals,
        capturable_primitive_literals: HashMap::new(),
        non_local_binding_decls: HashSet::new(),
        disable_memoization_features: options.disable_memoization_features,
        disable_memoization_for_debugging: options.disable_memoization_for_debugging,
        enable_change_variable_codegen: options.enable_change_variable_codegen,
        emit_hook_guards,
        enable_change_detection_for_debugging: options.enable_change_detection_for_debugging,
        enable_name_anonymous_functions: options.enable_name_anonymous_functions,
        needs_structural_check_import: false,
        function_name,
        param_display_names: HashMap::new(),
        reserved_child_decl_names: inherited_reserved_child_decl_names,
        block_scope_declared_temp_names: Vec::new(),
        declaration_name_overrides: HashMap::new(),
        used_declaration_names: HashSet::new(),
        preferred_decl_names: HashMap::new(),
        codegen_error: None,
        emitted_optional_dep_reads: HashSet::new(),
        pending_manual_memo_reads: HashSet::new(),
        manual_memo_root_decls,
        manual_memo_dep_roots_by_id: HashMap::new(),
        manual_memo_dep_roots_by_decl: HashMap::new(),
        pruned_manual_memo_decls: HashSet::new(),
        stable_zero_dep_decls: HashSet::new(),
        scope_dependency_decls,
        scope_dependency_overrides: HashMap::new(),
        function_decl_decls,
        jsx_only_component_tag_decls,
        inline_identifier_aliases: HashMap::new(),
        elided_named_declarations: HashSet::new(),
        preserve_loop_header_inits: false,
        block_scope_output_names: Vec::new(),
    };
    let fast_refresh_state = if options.enable_reset_cache_on_source_file_changes {
        get_fast_refresh_source_hash().map(|hash| (cx.alloc_cache_slot(), hash))
    } else {
        None
    };
    for (decl_id, name) in inherited_declaration_name_overrides {
        cx.declaration_name_overrides.insert(decl_id, name.clone());
        cx.used_declaration_names.insert(name.clone());
        cx.unique_identifiers.insert(name);
    }
    cx.preferred_decl_names = collect_preferred_declaration_names(&func.body);
    cx.non_local_binding_decls = collect_non_local_binding_declarations(&func.body);
    let mut hook_callback_arg_decls: HashSet<DeclarationId> = HashSet::new();
    collect_hook_callback_decl_ids_in_block(&mut cx, &func.body, &mut hook_callback_arg_decls);
    let mut decl_assignment_edges: Vec<(DeclarationId, DeclarationId)> = Vec::new();
    collect_declaration_assignment_edges_in_block(&func.body, &mut decl_assignment_edges);
    let mut changed = true;
    while changed {
        changed = false;
        for (source_decl, target_decl) in &decl_assignment_edges {
            if hook_callback_arg_decls.contains(target_decl)
                && hook_callback_arg_decls.insert(*source_decl)
            {
                changed = true;
            }
        }
    }
    if std::env::var("DEBUG_HOOK_CALLBACK_CANDIDATES").is_ok() {
        let mut ids: Vec<u32> = hook_callback_arg_decls.iter().map(|id| id.0).collect();
        ids.sort_unstable();
        eprintln!(
            "[HOOK_CALLBACK_CANDIDATES] fn={} ids={:?}",
            cx.function_name, ids
        );
    }
    cx.hook_callback_arg_decls = hook_callback_arg_decls;

    // Declare parameters
    let mut param_names: Vec<String> = Vec::new();
    for (param_index, param) in func.params.iter().enumerate() {
        let place = match param {
            Argument::Place(p) => p,
            Argument::Spread(p) => p,
        };
        if param_index == 1
            && place
                .identifier
                .name
                .as_ref()
                .is_some_and(|name| name.value() == "ref")
        {
            // ForwardRef-style second parameter behaves like a stable ref object.
            cx.mark_stable_ref_identifier(&place.identifier);
        }
        let raw_name = identifier_name_with_cx(&mut cx, &place.identifier);
        let param_name = if cx.disable_memoization_features && is_codegen_temp_name(&raw_name) {
            format!("t{}", param_index)
        } else {
            raw_name
        };
        cx.param_display_names
            .insert(place.identifier.declaration_id, param_name.clone());
        cx.declaration_name_overrides
            .insert(place.identifier.declaration_id, param_name.clone());
        cx.used_declaration_names.insert(param_name.clone());
        cx.declared_names.insert(param_name.clone());
        cx.set_temp_expr(&place.identifier, Some(ExprValue::primary(param_name)));
        if let Some(rendered) = cx.param_display_names.get(&place.identifier.declaration_id) {
            param_names.push(rendered.clone());
        }
        cx.declare(&place.identifier);
    }
    if cx.disable_memoization_features {
        let fire_binding_decls = collect_fire_binding_declarations(&func.body);
        if std::env::var("DEBUG_REACTIVE_SCOPE_NAMES").is_ok() && !fire_binding_decls.is_empty() {
            eprintln!(
                "[REACTIVE_FIRE_BINDINGS] fn={} decls={:?}",
                cx.function_name, fire_binding_decls
            );
        }
        for decl_id in fire_binding_decls {
            if cx.declaration_name_overrides.contains_key(&decl_id) {
                continue;
            }
            let fresh = fresh_temp_name(&mut cx);
            cx.declaration_name_overrides.insert(decl_id, fresh.clone());
            cx.used_declaration_names.insert(fresh);
        }
    }

    let body = codegen_block(&mut cx, &func.body);

    // Remove trailing bare `return;`
    let trimmed = body.trim_end();
    let mut body = if let Some(prefix) = trimmed.strip_suffix("return;") {
        let mut s = prefix.to_string();
        s.push('\n');
        s
    } else {
        body
    };
    let needs_function_hook_guard_wrapper = cx.emit_hook_guards && emit_function_hook_guard;
    if needs_function_hook_guard_wrapper && options.emit_function_hook_guard_wrapper_in_body {
        body = wrap_hook_guarded_block(&body, HOOK_GUARD_PUSH, HOOK_GUARD_POP);
    }
    body = prune_unused_const_literal_decls(&body);

    let cache_size = cx.next_cache_index;
    let needs_cache_import = cache_size > 0;

    // Build output with correct ordering: directives, then cache allocation, then body.
    // Upstream puts directives like "use no forget" before `const $ = _c(N);`.
    let cache_prologue = if cache_size > 0 {
        Some(CachePrologue {
            binding_name: cx.synthesize_name("$"),
            size: cache_size,
            fast_refresh: fast_refresh_state.as_ref().map(|(cache_index, hash)| {
                FastRefreshPrologue {
                    cache_index: *cache_index,
                    hash: hash.clone(),
                    index_binding_name: cx.synthesize_name("$i"),
                }
            }),
        })
    } else {
        None
    };

    let mut output = String::new();
    if let Some(prologue) = render_reactive_function_body_prologue_ast(
        if options.emit_directives_in_body {
            Some(&func.directives)
        } else {
            None
        },
        cache_prologue.as_ref(),
    ) {
        output.push_str(&prologue);
    }
    output.push_str(&body);

    if std::env::var("DEBUG_REACTIVE_RAW").is_ok() {
        eprintln!(
            "[REACTIVE_RAW_OUTPUT] function={:?}\n{}",
            func.name_hint.as_deref(),
            output
        );
    }

    CodegenResult {
        body: output,
        cache_size,
        needs_cache_import,
        param_names,
        needs_freeze_import: false,
        has_fire_rewrite: false,
        needs_hook_guards: body.contains(HOOK_GUARD_IDENT) || needs_function_hook_guard_wrapper,
        needs_function_hook_guard_wrapper,
        needs_structural_check_import: cx.needs_structural_check_import,
        cache_prologue,
        error: cx.codegen_error,
    }
}

fn prune_unused_const_literal_decls(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    if lines.is_empty() {
        return body.to_string();
    }

    let mut in_multiline_for_header = vec![false; lines.len()];
    let mut for_header_start: Option<usize> = None;
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some(start) = for_header_start {
            if trimmed == ") {" || trimmed == ")" || trimmed.starts_with(") {") {
                for slot in in_multiline_for_header.iter_mut().take(idx + 1).skip(start) {
                    *slot = true;
                }
                for_header_start = None;
            }
            continue;
        }
        if trimmed == "for (" || trimmed.ends_with("for (") {
            for_header_start = Some(idx);
        }
    }

    let mut candidates: HashMap<usize, String> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        if in_multiline_for_header[idx] {
            continue;
        }
        let trimmed = line.trim();
        let Some((name, rhs)) = parse_const_literal_decl(trimmed) else {
            continue;
        };
        if is_inlineable_primitive_literal_expression(rhs) {
            candidates.insert(idx, name.to_string());
        }
    }
    if candidates.is_empty() {
        return body.to_string();
    }

    let mut keep = vec![true; lines.len()];
    for (idx, name) in candidates {
        let used_elsewhere = lines
            .iter()
            .enumerate()
            .any(|(other, line)| other != idx && contains_identifier_token(line, &name));
        if !used_elsewhere {
            keep[idx] = false;
        }
    }

    let mut out = String::new();
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn parse_const_literal_decl(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("const ")?;
    let eq = rest.find('=')?;
    if eq == 0 || eq + 1 >= rest.len() || !rest.ends_with(';') {
        return None;
    }
    let name = rest[..eq].trim();
    if !is_simple_identifier_name(name) {
        return None;
    }
    let rhs = rest[eq + 1..rest.len() - 1].trim();
    if rhs.is_empty() {
        return None;
    }
    Some((name, rhs))
}

fn contains_identifier_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0usize;
    while let Some(found) = haystack[start..].find(needle) {
        let idx = start + found;
        let before_ok = if idx == 0 {
            true
        } else {
            !is_simple_identifier_char(haystack.as_bytes()[idx - 1] as char)
        };
        let end = idx + needle.len();
        let after_ok = if end >= haystack.len() {
            true
        } else {
            !is_simple_identifier_char(haystack.as_bytes()[end] as char)
        };
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

fn contains_base_identifier_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0usize;
    while let Some(found) = haystack[start..].find(needle) {
        let idx = start + found;
        let before_ok = if idx == 0 {
            true
        } else {
            let prev = haystack.as_bytes()[idx - 1] as char;
            prev != '.' && !is_simple_identifier_char(prev)
        };
        let end = idx + needle.len();
        let after_ok = if end >= haystack.len() {
            true
        } else {
            !is_simple_identifier_char(haystack.as_bytes()[end] as char)
        };
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}

fn is_simple_identifier_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_simple_identifier_char(first) || first.is_ascii_digit() {
        return false;
    }
    chars.all(is_simple_identifier_char)
}

fn is_simple_identifier_char(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn wrap_hook_guarded_block(body: &str, before: u8, after: u8) -> String {
    let mut out = String::new();
    out.push_str("try {\n");
    out.push_str(&format!("{}({});\n", HOOK_GUARD_IDENT, before));
    if !body.is_empty() {
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str("} finally {\n");
    out.push_str(&format!("{}({});\n", HOOK_GUARD_IDENT, after));
    out.push_str("}\n");
    out
}

fn wrap_hook_guarded_call_expression(call_expr: &str) -> String {
    format!(
        "(function () {{\ntry {{\n{}({});\nreturn {};\n}} finally {{\n{}({});\n}}\n}})()",
        HOOK_GUARD_IDENT, HOOK_GUARD_ALLOW, call_expr, HOOK_GUARD_IDENT, HOOK_GUARD_DISALLOW
    )
}

fn set_codegen_error_once(cx: &mut Context, reason: &str, message: String) {
    if cx.codegen_error.is_some() {
        return;
    }
    cx.codegen_error = Some(CompilerError::Bail(BailOut {
        reason: reason.to_string(),
        diagnostics: vec![CompilerDiagnostic {
            severity: DiagnosticSeverity::Invariant,
            message,
        }],
    }));
}

fn set_const_declaration_expression_error(cx: &mut Context, kind: InstructionKind) {
    let message = if kind == InstructionKind::Let || kind == InstructionKind::HoistedLet {
        "this is const".to_string()
    } else {
        format!("this is {:?}", kind)
    };
    set_codegen_error_once(
        cx,
        "Const declaration cannot be referenced as an expression",
        message,
    );
}

fn set_function_declaration_expression_error(cx: &mut Context, kind: InstructionKind) {
    set_codegen_error_once(
        cx,
        "Function declaration cannot be referenced as an expression",
        format!("this is {:?}", kind),
    );
}

fn adopt_codegen_error(cx: &mut Context, child_error: Option<CompilerError>) {
    if cx.codegen_error.is_none() {
        cx.codegen_error = child_error;
    }
}

fn collect_child_context_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut captured = HashSet::new();
    collect_child_context_declarations_in_block(block, &mut captured);
    captured
}

fn collect_mutable_child_context_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut captured = HashSet::new();
    collect_mutable_child_context_declarations_in_block(block, &mut captured);
    captured
}

fn collect_child_context_declarations_in_block(
    block: &ReactiveBlock,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    for place in &lowered_func.func.context {
                        out.insert(place.identifier.declaration_id);
                    }
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope) => {
                collect_child_context_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_child_context_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_child_context_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_child_context_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_child_context_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_child_context_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_child_context_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_child_context_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_child_context_declarations_in_block(block, out);
            collect_child_context_declarations_in_block(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_mutable_child_context_declarations_in_block(
    block: &ReactiveBlock,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    let mut child_reassigned = HashSet::new();
                    collect_reassigned_decl_ids_in_hir_function(
                        &lowered_func.func,
                        &mut child_reassigned,
                    );
                    for place in &lowered_func.func.context {
                        if child_reassigned.contains(&place.identifier.declaration_id) {
                            out.insert(place.identifier.declaration_id);
                        }
                    }
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope) => {
                collect_mutable_child_context_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_mutable_child_context_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_mutable_child_context_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_reassigned_decl_ids_in_hir_function(
    func: &HIRFunction,
    out: &mut HashSet<DeclarationId>,
) {
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::PrefixUpdate { lvalue, .. }
                | InstructionValue::PostfixUpdate { lvalue, .. } => {
                    out.insert(lvalue.identifier.declaration_id);
                }
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_reassigned_decl_ids_in_hir_function(&lowered_func.func, out);
                }
                _ => {}
            }
        }
    }
}

fn collect_mutable_child_context_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_mutable_child_context_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_mutable_child_context_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_mutable_child_context_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_mutable_child_context_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_mutable_child_context_declarations_in_block(block, out);
            collect_mutable_child_context_declarations_in_block(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_reassigned_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut reassigned = HashSet::new();
    collect_reassigned_declarations_in_block(block, &mut reassigned);
    reassigned
}

fn collect_reassigned_declarations_in_block(
    block: &ReactiveBlock,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Reassign {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope) => {
                collect_reassigned_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_reassigned_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_reassigned_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_reassigned_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_reassigned_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_reassigned_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_reassigned_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_reassigned_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_reassigned_declarations_in_block(block, out);
            collect_reassigned_declarations_in_block(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_read_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut reads = HashSet::new();
    collect_read_declarations_in_block(block, &mut reads);
    reads
}

fn collect_scope_dependency_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut deps = HashSet::new();
    collect_scope_dependency_declarations_in_block(block, &mut deps);
    deps
}

fn collect_scope_dependency_declarations_in_block(
    block: &ReactiveBlock,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(_) => {}
            ReactiveStatement::Scope(scope) => {
                for dep in &scope.scope.dependencies {
                    out.insert(dep.identifier.declaration_id);
                }
                collect_scope_dependency_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                for dep in &scope.scope.dependencies {
                    out.insert(dep.identifier.declaration_id);
                }
                collect_scope_dependency_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_scope_dependency_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_scope_dependency_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        ReactiveTerminal::Return { .. } | ReactiveTerminal::Throw { .. } => {}
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_scope_dependency_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_scope_dependency_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_scope_dependency_declarations_in_block(init, out);
            if let Some(update) = update {
                collect_scope_dependency_declarations_in_block(update, out);
            }
            collect_scope_dependency_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_scope_dependency_declarations_in_block(init, out);
            collect_scope_dependency_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_scope_dependency_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_scope_dependency_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_scope_dependency_declarations_in_block(block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_scope_dependency_declarations_in_block(block, out);
            collect_scope_dependency_declarations_in_block(handler, out);
        }
    }
}

fn collect_non_local_binding_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut direct_non_local = HashSet::new();
    let mut aliases: Vec<(DeclarationId, DeclarationId)> = Vec::new();
    collect_non_local_binding_declarations_in_block(block, &mut direct_non_local, &mut aliases);

    // Propagate non-local origin through lowered alias loads before memo guard
    // generation. This keeps dep filtering stable even when upstream-style
    // expression fusion removes explicit LoadGlobal emissions.
    let mut propagated = direct_non_local;
    let mut changed = true;
    while changed {
        changed = false;
        for (target, source) in &aliases {
            if propagated.contains(source) && propagated.insert(*target) {
                changed = true;
            }
        }
    }
    propagated
}

fn collect_non_local_binding_declarations_in_block(
    block: &ReactiveBlock,
    direct_non_local: &mut HashSet<DeclarationId>,
    aliases: &mut Vec<(DeclarationId, DeclarationId)>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                let Some(lvalue) = &instr.lvalue else {
                    continue;
                };
                match &instr.value {
                    InstructionValue::LoadGlobal { .. } => {
                        direct_non_local.insert(lvalue.identifier.declaration_id);
                    }
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        aliases.push((
                            lvalue.identifier.declaration_id,
                            place.identifier.declaration_id,
                        ));
                    }
                    InstructionValue::TypeCastExpression { value, .. } => {
                        aliases.push((
                            lvalue.identifier.declaration_id,
                            value.identifier.declaration_id,
                        ));
                    }
                    _ => {}
                }
            }
            ReactiveStatement::Scope(scope) => {
                collect_non_local_binding_declarations_in_block(
                    &scope.instructions,
                    direct_non_local,
                    aliases,
                );
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_non_local_binding_declarations_in_block(
                    &scope.instructions,
                    direct_non_local,
                    aliases,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_non_local_binding_declarations_in_terminal(
                    &term_stmt.terminal,
                    direct_non_local,
                    aliases,
                );
            }
        }
    }
}

fn collect_non_local_binding_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    direct_non_local: &mut HashSet<DeclarationId>,
    aliases: &mut Vec<(DeclarationId, DeclarationId)>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_non_local_binding_declarations_in_block(consequent, direct_non_local, aliases);
            if let Some(alt) = alternate {
                collect_non_local_binding_declarations_in_block(alt, direct_non_local, aliases);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_non_local_binding_declarations_in_block(
                        block,
                        direct_non_local,
                        aliases,
                    );
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_non_local_binding_declarations_in_block(init, direct_non_local, aliases);
            if let Some(update_block) = update {
                collect_non_local_binding_declarations_in_block(
                    update_block,
                    direct_non_local,
                    aliases,
                );
            }
            collect_non_local_binding_declarations_in_block(loop_block, direct_non_local, aliases);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_non_local_binding_declarations_in_block(init, direct_non_local, aliases);
            collect_non_local_binding_declarations_in_block(loop_block, direct_non_local, aliases);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_non_local_binding_declarations_in_block(loop_block, direct_non_local, aliases);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_non_local_binding_declarations_in_block(block, direct_non_local, aliases);
            collect_non_local_binding_declarations_in_block(handler, direct_non_local, aliases);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_function_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut function_expr_decl_captures = HashMap::new();
    collect_function_expression_declaration_metadata_in_block(
        block,
        &mut function_expr_decl_captures,
    );

    let mut function_decls = HashSet::new();
    collect_function_declarations_in_block(
        block,
        &function_expr_decl_captures,
        &mut function_decls,
    );
    function_decls
}

fn collect_function_expression_declaration_metadata_in_block(
    block: &ReactiveBlock,
    out: &mut HashMap<DeclarationId, HashSet<DeclarationId>>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let InstructionValue::FunctionExpression {
                    expr_type,
                    lowered_func,
                    ..
                } = &instr.value
                    && *expr_type == FunctionExpressionType::FunctionDeclaration
                    && let Some(lvalue) = &instr.lvalue
                {
                    let captures = lowered_func
                        .func
                        .context
                        .iter()
                        .map(|place| place.identifier.declaration_id)
                        .collect::<HashSet<_>>();
                    out.insert(lvalue.identifier.declaration_id, captures);
                }
            }
            ReactiveStatement::Scope(scope) => {
                collect_function_expression_declaration_metadata_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_function_expression_declaration_metadata_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_function_expression_declaration_metadata_in_terminal(
                    &term_stmt.terminal,
                    out,
                );
            }
        }
    }
}

fn collect_function_expression_declaration_metadata_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashMap<DeclarationId, HashSet<DeclarationId>>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_function_expression_declaration_metadata_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_function_expression_declaration_metadata_in_block(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_function_expression_declaration_metadata_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_function_expression_declaration_metadata_in_block(init, out);
            if let Some(update_block) = update {
                collect_function_expression_declaration_metadata_in_block(update_block, out);
            }
            collect_function_expression_declaration_metadata_in_block(loop_block, out);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_function_expression_declaration_metadata_in_block(init, out);
            collect_function_expression_declaration_metadata_in_block(loop_block, out);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_function_expression_declaration_metadata_in_block(loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_function_expression_declaration_metadata_in_block(block, out);
            collect_function_expression_declaration_metadata_in_block(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_function_declarations_in_block(
    block: &ReactiveBlock,
    function_expr_decl_captures: &HashMap<DeclarationId, HashSet<DeclarationId>>,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    if lvalue.kind == InstructionKind::Function {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    if lvalue.place.identifier.name.is_some()
                        && function_expr_decl_captures
                            .get(&value.identifier.declaration_id)
                            .is_some_and(|captures| {
                                captures.contains(&lvalue.place.identifier.declaration_id)
                            })
                    {
                        out.insert(lvalue.place.identifier.declaration_id);
                    }
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope) => {
                collect_function_declarations_in_block(
                    &scope.instructions,
                    function_expr_decl_captures,
                    out,
                );
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_function_declarations_in_block(
                    &scope.instructions,
                    function_expr_decl_captures,
                    out,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_function_declarations_in_terminal(
                    &term_stmt.terminal,
                    function_expr_decl_captures,
                    out,
                );
            }
        }
    }
}

fn collect_function_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    function_expr_decl_captures: &HashMap<DeclarationId, HashSet<DeclarationId>>,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_function_declarations_in_block(consequent, function_expr_decl_captures, out);
            if let Some(alt) = alternate {
                collect_function_declarations_in_block(alt, function_expr_decl_captures, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_function_declarations_in_block(block, function_expr_decl_captures, out);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_function_declarations_in_block(init, function_expr_decl_captures, out);
            if let Some(update_block) = update {
                collect_function_declarations_in_block(
                    update_block,
                    function_expr_decl_captures,
                    out,
                );
            }
            collect_function_declarations_in_block(loop_block, function_expr_decl_captures, out);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_function_declarations_in_block(init, function_expr_decl_captures, out);
            collect_function_declarations_in_block(loop_block, function_expr_decl_captures, out);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_function_declarations_in_block(loop_block, function_expr_decl_captures, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_function_declarations_in_block(block, function_expr_decl_captures, out);
            collect_function_declarations_in_block(handler, function_expr_decl_captures, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_preferred_declaration_names(block: &ReactiveBlock) -> HashMap<DeclarationId, String> {
    let mut names = HashMap::new();
    collect_preferred_declaration_names_in_block(block, &mut names);
    names
}

fn collect_fire_binding_declarations(block: &ReactiveBlock) -> Vec<DeclarationId> {
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    collect_fire_binding_declarations_in_block(block, &mut ordered, &mut seen);
    ordered
}

fn collect_fire_binding_declarations_in_block(
    block: &ReactiveBlock,
    ordered: &mut Vec<DeclarationId>,
    seen: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(lvalue) = &instr.lvalue
                    && is_fire_binding_identifier(&lvalue.identifier)
                    && seen.insert(lvalue.identifier.declaration_id)
                {
                    ordered.push(lvalue.identifier.declaration_id);
                }
            }
            ReactiveStatement::Scope(scope) => {
                collect_fire_binding_declarations_in_block(&scope.instructions, ordered, seen);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_fire_binding_declarations_in_block(&scope.instructions, ordered, seen);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_fire_binding_declarations_in_terminal(&term_stmt.terminal, ordered, seen);
            }
        }
    }
}

fn collect_fire_binding_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    ordered: &mut Vec<DeclarationId>,
    seen: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_fire_binding_declarations_in_block(consequent, ordered, seen);
            if let Some(alt) = alternate {
                collect_fire_binding_declarations_in_block(alt, ordered, seen);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_fire_binding_declarations_in_block(block, ordered, seen);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_fire_binding_declarations_in_block(init, ordered, seen);
            if let Some(update_block) = update {
                collect_fire_binding_declarations_in_block(update_block, ordered, seen);
            }
            collect_fire_binding_declarations_in_block(loop_block, ordered, seen);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_fire_binding_declarations_in_block(init, ordered, seen);
            collect_fire_binding_declarations_in_block(loop_block, ordered, seen);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_fire_binding_declarations_in_block(loop_block, ordered, seen);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_fire_binding_declarations_in_block(block, ordered, seen);
            collect_fire_binding_declarations_in_block(handler, ordered, seen);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn is_fire_binding_identifier(identifier: &Identifier) -> bool {
    matches!(
        &identifier.type_,
        Type::Function {
            shape_id: Some(shape_id),
            ..
        } if shape_id == "BuiltInFireFunction"
    )
}

fn remember_preferred_identifier_name(
    names: &mut HashMap<DeclarationId, String>,
    identifier: &Identifier,
) {
    let Some(name) = identifier.name.as_ref() else {
        return;
    };
    names
        .entry(identifier.declaration_id)
        .or_insert_with(|| name.value().to_string());
}

fn collect_preferred_declaration_names_in_block(
    block: &ReactiveBlock,
    names: &mut HashMap<DeclarationId, String>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(lvalue) = &instr.lvalue {
                    remember_preferred_identifier_name(names, &lvalue.identifier);
                }
            }
            ReactiveStatement::Scope(scope) => {
                collect_preferred_declaration_names_in_block(&scope.instructions, names);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_preferred_declaration_names_in_block(&scope.instructions, names);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_preferred_declaration_names_in_terminal(&term_stmt.terminal, names);
            }
        }
    }
}

fn collect_preferred_declaration_names_in_terminal(
    terminal: &ReactiveTerminal,
    names: &mut HashMap<DeclarationId, String>,
) {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            remember_preferred_identifier_name(names, &value.identifier);
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            remember_preferred_identifier_name(names, &test.identifier);
            for case in cases {
                if let Some(test) = &case.test {
                    remember_preferred_identifier_name(names, &test.identifier);
                }
                if let Some(block) = &case.block {
                    collect_preferred_declaration_names_in_block(block, names);
                }
            }
        }
        ReactiveTerminal::DoWhile {
            loop_block, test, ..
        }
        | ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            remember_preferred_identifier_name(names, &test.identifier);
            collect_preferred_declaration_names_in_block(loop_block, names);
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            collect_preferred_declaration_names_in_block(init, names);
            remember_preferred_identifier_name(names, &test.identifier);
            if let Some(update) = update {
                collect_preferred_declaration_names_in_block(update, names);
            }
            collect_preferred_declaration_names_in_block(loop_block, names);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            collect_preferred_declaration_names_in_block(init, names);
            remember_preferred_identifier_name(names, &test.identifier);
            collect_preferred_declaration_names_in_block(loop_block, names);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_preferred_declaration_names_in_block(init, names);
            collect_preferred_declaration_names_in_block(loop_block, names);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            remember_preferred_identifier_name(names, &test.identifier);
            collect_preferred_declaration_names_in_block(consequent, names);
            if let Some(alt) = alternate {
                collect_preferred_declaration_names_in_block(alt, names);
            }
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_preferred_declaration_names_in_block(block, names);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            if let Some(binding) = handler_binding {
                remember_preferred_identifier_name(names, &binding.identifier);
            }
            collect_preferred_declaration_names_in_block(block, names);
            collect_preferred_declaration_names_in_block(handler, names);
        }
    }
}

fn collect_read_declarations_in_block(block: &ReactiveBlock, out: &mut HashSet<DeclarationId>) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                visitors::for_each_instruction_value_operand(&instr.value, |place| {
                    out.insert(place.identifier.declaration_id);
                });
            }
            ReactiveStatement::Scope(scope) => {
                collect_read_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_read_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_read_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_read_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => {}
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            out.insert(value.identifier.declaration_id);
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            out.insert(test.identifier.declaration_id);
            for case in cases {
                if let Some(test) = &case.test {
                    out.insert(test.identifier.declaration_id);
                }
                if let Some(block) = &case.block {
                    collect_read_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::DoWhile {
            loop_block, test, ..
        }
        | ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            out.insert(test.identifier.declaration_id);
            collect_read_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            collect_read_declarations_in_block(init, out);
            out.insert(test.identifier.declaration_id);
            if let Some(update) = update {
                collect_read_declarations_in_block(update, out);
            }
            collect_read_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            collect_read_declarations_in_block(init, out);
            out.insert(test.identifier.declaration_id);
            collect_read_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_read_declarations_in_block(init, out);
            collect_read_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            out.insert(test.identifier.declaration_id);
            collect_read_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_read_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_read_declarations_in_block(block, out);
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            if let Some(binding) = handler_binding {
                out.insert(binding.identifier.declaration_id);
            }
            collect_read_declarations_in_block(block, out);
            collect_read_declarations_in_block(handler, out);
        }
    }
}

fn collect_manual_memo_root_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut roots = HashSet::new();
    collect_manual_memo_root_declarations_in_block(block, &mut roots);
    roots
}

fn collect_manual_memo_root_declarations_in_block(
    block: &ReactiveBlock,
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let InstructionValue::StartMemoize { deps, .. } = &instr.value
                    && let Some(deps) = deps
                {
                    for dep in deps {
                        if let ManualMemoRoot::NamedLocal(place) = &dep.root {
                            out.insert(place.identifier.declaration_id);
                        }
                    }
                }
            }
            ReactiveStatement::Scope(scope) => {
                collect_manual_memo_root_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_manual_memo_root_declarations_in_block(&scope.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_manual_memo_root_declarations_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_manual_memo_root_declarations_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_manual_memo_root_declarations_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_manual_memo_root_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_manual_memo_root_declarations_in_block(init, out);
            if let Some(update) = update {
                collect_manual_memo_root_declarations_in_block(update, out);
            }
            collect_manual_memo_root_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_manual_memo_root_declarations_in_block(init, out);
            collect_manual_memo_root_declarations_in_block(loop_block, out);
        }
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_manual_memo_root_declarations_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_manual_memo_root_declarations_in_block(alt, out);
            }
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_manual_memo_root_declarations_in_block(block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_manual_memo_root_declarations_in_block(block, out);
            collect_manual_memo_root_declarations_in_block(handler, out);
        }
    }
}

fn collect_jsx_only_component_tag_declarations(block: &ReactiveBlock) -> HashSet<DeclarationId> {
    let mut jsx_component_reads = HashSet::new();
    let mut non_jsx_reads = HashSet::new();
    collect_jsx_tag_usage_in_block(block, &mut jsx_component_reads, &mut non_jsx_reads);
    jsx_component_reads.retain(|decl_id| !non_jsx_reads.contains(decl_id));
    jsx_component_reads
}

fn collect_jsx_tag_usage_in_block(
    block: &ReactiveBlock,
    jsx_component_reads: &mut HashSet<DeclarationId>,
    non_jsx_reads: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::JsxExpression {
                    tag,
                    props,
                    children,
                    ..
                } => {
                    if let JsxTag::Component(place) = tag {
                        jsx_component_reads.insert(place.identifier.declaration_id);
                    }
                    for prop in props {
                        match prop {
                            JsxAttribute::Attribute { place, .. }
                            | JsxAttribute::SpreadAttribute { argument: place } => {
                                non_jsx_reads.insert(place.identifier.declaration_id);
                            }
                        }
                    }
                    if let Some(children) = children {
                        for child in children {
                            non_jsx_reads.insert(child.identifier.declaration_id);
                        }
                    }
                }
                _ => {
                    visitors::for_each_instruction_value_operand(&instr.value, |place| {
                        non_jsx_reads.insert(place.identifier.declaration_id);
                    });
                }
            },
            ReactiveStatement::Scope(scope) => {
                collect_jsx_tag_usage_in_block(
                    &scope.instructions,
                    jsx_component_reads,
                    non_jsx_reads,
                );
            }
            ReactiveStatement::PrunedScope(scope) => {
                collect_jsx_tag_usage_in_block(
                    &scope.instructions,
                    jsx_component_reads,
                    non_jsx_reads,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_read_declarations_in_terminal(&term_stmt.terminal, non_jsx_reads);
            }
        }
    }
}

/// Generate a block of statements, resetting temporaries afterward.
fn codegen_block(cx: &mut Context, block: &[ReactiveStatement]) -> String {
    let temp_snapshot = cx.snapshot_temps();
    cx.block_scope_output_names.push(HashSet::new());
    cx.block_scope_declared_temp_names.push(HashSet::new());
    let result = codegen_block_no_reset(cx, block);
    let _ = cx.block_scope_declared_temp_names.pop();
    let _ = cx.block_scope_output_names.pop();
    cx.restore_temps(temp_snapshot);
    result
}

/// Generate a block of statements without resetting temporaries.
/// Used for sequence expressions where the final value references earlier temps.
fn codegen_block_no_reset(cx: &mut Context, block: &[ReactiveStatement]) -> String {
    codegen_block_no_reset_with_options(cx, block, false)
}

fn codegen_block_no_reset_with_options(
    cx: &mut Context,
    block: &[ReactiveStatement],
    allow_top_level_zero_dep_literal_inline: bool,
) -> String {
    #[derive(Clone)]
    struct PendingSequenceExpr {
        exprs: Vec<String>,
        loc: SourceLocation,
    }

    fn source_loc_line_range(loc: &SourceLocation) -> Option<(u32, u32)> {
        match loc {
            SourceLocation::Source(range) => Some((range.start.line, range.end.line)),
            SourceLocation::Generated => None,
        }
    }

    fn maybe_insert_source_line_gap(
        output: &mut String,
        last_source_end_line: &mut Option<u32>,
        current_loc: Option<&SourceLocation>,
    ) {
        let Some(current_loc) = current_loc else {
            return;
        };
        let Some((current_start_line, _)) = source_loc_line_range(current_loc) else {
            return;
        };
        let Some(previous_end_line) = *last_source_end_line else {
            return;
        };
        if current_start_line > previous_end_line + 1 && !output.ends_with("\n\n") {
            if !output.ends_with('\n') {
                output.push('\n');
            }
            output.push('\n');
        }
    }

    fn should_insert_blank_line_after_function_assignment(stmt: &str) -> bool {
        let trimmed = stmt.trim_start();
        let trimmed_no_trailing = trimmed.trim_end();
        if !(trimmed.starts_with("const ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("var "))
        {
            return false;
        }
        if !trimmed_no_trailing.contains('\n') {
            return false;
        }
        trimmed.contains("= function")
            || trimmed.contains("= async function")
            || trimmed.contains("=>")
    }

    fn append_statement_with_source_gap(
        output: &mut String,
        stmt: &str,
        current_loc: Option<&SourceLocation>,
        last_source_end_line: &mut Option<u32>,
    ) {
        maybe_insert_source_line_gap(output, last_source_end_line, current_loc);
        output.push_str(stmt);
        if !stmt.ends_with('\n') {
            output.push('\n');
        }
        if should_insert_blank_line_after_function_assignment(stmt) && !output.ends_with("\n\n") {
            output.push('\n');
        }
        if let Some(current_loc) = current_loc
            && let Some((_, current_end_line)) = source_loc_line_range(current_loc)
        {
            *last_source_end_line = Some(current_end_line);
        }
    }

    fn is_simple_identifier(name: &str) -> bool {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
            return false;
        }
        chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
    }

    fn parse_uninitialized_declaration_name(stmt: &str) -> Option<String> {
        let stmt = stmt.trim().trim_end_matches(';').trim();
        let body = stmt
            .strip_prefix("let ")
            .or_else(|| stmt.strip_prefix("const "))
            .or_else(|| stmt.strip_prefix("var "))?
            .trim();
        if body.contains('=') || body.contains(',') || !is_simple_identifier(body) {
            return None;
        }
        Some(body.to_string())
    }

    fn parse_simple_assignment(stmt: &str) -> Option<(String, String)> {
        let stmt = stmt.trim().trim_end_matches(';').trim();
        if let Some(body) = stmt
            .strip_prefix("let ")
            .or_else(|| stmt.strip_prefix("const "))
            .or_else(|| stmt.strip_prefix("var "))
        {
            let (lhs, rhs) = body.split_once('=')?;
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            if !is_simple_identifier(lhs) {
                return None;
            }
            return Some((lhs.to_string(), rhs.to_string()));
        }
        let (lhs, rhs) = stmt.split_once('=')?;
        let lhs = lhs.trim();
        let rhs = rhs.trim();
        if !is_simple_identifier(lhs) {
            return None;
        }
        Some((lhs.to_string(), rhs.to_string()))
    }

    enum NextEmittedStatement {
        PushOnTarget,
        TerminalIf { multiline_source: bool },
        Other,
    }

    fn next_emitted_statement_for_spacing(
        cx: &Context,
        block: &[ReactiveStatement],
        start_idx: usize,
        assignment_target_name: Option<&str>,
    ) -> Option<NextEmittedStatement> {
        let debug = std::env::var("DEBUG_STRUCTURAL_GAP").is_ok();
        fn resolve_place_simple_identifier(cx: &Context, place: &Place) -> Option<String> {
            if let Some(name) = resolve_place_name(cx, place)
                && is_simple_identifier(&name)
            {
                return Some(name);
            }
            cx.temp
                .get(&place.identifier.declaration_id)
                .and_then(|value| value.as_ref())
                .and_then(|value| {
                    let expr = value.expr.trim();
                    if is_simple_identifier(expr) {
                        Some(expr.to_string())
                    } else {
                        None
                    }
                })
        }

        fn is_push_method_property(cx: &Context, property: &Place) -> bool {
            if let Some(name) = resolve_place_name(cx, property)
                && (name == "push" || name.ends_with(".push") || name.ends_with("?.push"))
            {
                return true;
            }
            cx.temp
                .get(&property.identifier.declaration_id)
                .and_then(|value| value.as_ref())
                .is_some_and(|value| {
                    let expr = value.expr.trim();
                    expr == "\"push\"" || expr == "'push'" || expr == "push"
                })
        }

        let mut cursor = start_idx;
        while let Some(stmt) = block.get(cursor) {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if let Some(target_name) = assignment_target_name
                        && let InstructionValue::MethodCall {
                            receiver, property, ..
                        } = &instr.value
                    {
                        let receiver_name = resolve_place_simple_identifier(cx, receiver);
                        let property_name = resolve_place_name(cx, property);
                        let property_temp = cx
                            .temp
                            .get(&property.identifier.declaration_id)
                            .and_then(|value| value.as_ref())
                            .map(|value| value.expr.clone());
                        let property_is_push = is_push_method_property(cx, property);
                        if debug {
                            eprintln!(
                                "[STRUCT_GAP] lookahead methodcall idx={} receiver={:?} target={} prop_name={:?} prop_temp={:?} push={}",
                                cursor,
                                receiver_name,
                                target_name,
                                property_name,
                                property_temp,
                                property_is_push
                            );
                        }
                        if receiver_name
                            .as_deref()
                            .is_some_and(|name| name == target_name)
                            && property_is_push
                        {
                            return Some(NextEmittedStatement::PushOnTarget);
                        }
                    }
                    if instr.lvalue.is_none() {
                        if debug {
                            eprintln!(
                                "[STRUCT_GAP] lookahead stop idx={} kind={} lvalue-none",
                                cursor,
                                instruction_value_tag(&instr.value)
                            );
                        }
                        return Some(NextEmittedStatement::Other);
                    }
                    if debug {
                        eprintln!(
                            "[STRUCT_GAP] lookahead continue idx={} kind={} lvalue-some",
                            cursor,
                            instruction_value_tag(&instr.value)
                        );
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    if let ReactiveTerminal::If { loc, .. } = &term_stmt.terminal {
                        let multiline_source = matches!(loc, SourceLocation::Source(range) if range.end.line > range.start.line);
                        if debug {
                            eprintln!(
                                "[STRUCT_GAP] lookahead terminal-if idx={} multiline_source={}",
                                cursor, multiline_source
                            );
                        }
                        return Some(NextEmittedStatement::TerminalIf { multiline_source });
                    }
                    if debug {
                        eprintln!("[STRUCT_GAP] lookahead terminal-other idx={}", cursor);
                    }
                    return Some(NextEmittedStatement::Other);
                }
                ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_) => {
                    if debug {
                        eprintln!("[STRUCT_GAP] lookahead stop idx={} scope", cursor);
                    }
                    return None;
                }
            }
            cursor += 1;
        }
        None
    }

    fn should_insert_structural_blank_after_instruction(
        cx: &Context,
        block: &[ReactiveStatement],
        idx: usize,
        stmt: &str,
        current_loc: Option<&SourceLocation>,
    ) -> bool {
        let debug = std::env::var("DEBUG_STRUCTURAL_GAP").is_ok();
        let is_generated = matches!(current_loc, Some(SourceLocation::Generated));
        let source_span_lines = match current_loc {
            Some(SourceLocation::Source(range)) => Some((range.start.line, range.end.line)),
            _ => None,
        };
        let is_multiline_source =
            source_span_lines.is_some_and(|(start_line, end_line)| end_line > start_line);
        let uninitialized_decl = if matches!(current_loc, Some(SourceLocation::Generated)) {
            parse_uninitialized_declaration_name(stmt)
        } else {
            None
        };
        let assignment = parse_simple_assignment(stmt);
        if uninitialized_decl.is_none() && assignment.is_none() {
            if debug {
                eprintln!(
                    "[STRUCT_GAP] idx={} skip: not candidate stmt={:?}",
                    idx,
                    stmt.trim()
                );
            }
            return false;
        }

        let assignment_target_name = assignment.as_ref().map(|(name, _)| name.as_str());
        let next_stmt =
            next_emitted_statement_for_spacing(cx, block, idx + 1, assignment_target_name);
        if debug {
            eprintln!(
                "[STRUCT_GAP] idx={} stmt={:?} loc={:?} uninit={:?} assign={:?} next={}",
                idx,
                stmt.trim(),
                source_span_lines,
                uninitialized_decl,
                assignment,
                match &next_stmt {
                    Some(NextEmittedStatement::PushOnTarget) => "push",
                    Some(NextEmittedStatement::TerminalIf {
                        multiline_source: true,
                    }) => {
                        "if(multiline)"
                    }
                    Some(NextEmittedStatement::TerminalIf {
                        multiline_source: false,
                    }) => {
                        "if(singleline)"
                    }
                    Some(NextEmittedStatement::Other) => "other",
                    None => "none",
                }
            );
        }
        if uninitialized_decl
            .as_deref()
            .is_some_and(|name| matches!(name.strip_prefix('t'), Some(rest) if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())))
            && matches!(
                next_stmt,
                Some(NextEmittedStatement::TerminalIf {
                    multiline_source: true
                })
            )
        {
            if debug {
                eprintln!("[STRUCT_GAP] idx={} decision: blank-after-uninit-if", idx);
            }
            return true;
        }

        if !(is_generated || is_multiline_source) {
            if debug {
                eprintln!(
                    "[STRUCT_GAP] idx={} decision: single-line source assignment, skip",
                    idx
                );
            }
            return false;
        }
        let is_declaration_assignment = stmt.trim_start().starts_with("let ")
            || stmt.trim_start().starts_with("const ")
            || stmt.trim_start().starts_with("var ");
        if is_declaration_assignment {
            if debug {
                eprintln!(
                    "[STRUCT_GAP] idx={} decision: declaration assignment, skip",
                    idx
                );
            }
            return false;
        }
        let Some((name, rhs)) = assignment else {
            return false;
        };
        let rhs_is_iife_like = rhs == "[]"
            || rhs.contains("?? []")
            || matches!(rhs.strip_prefix('t'), Some(rest) if !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()));
        if !rhs_is_iife_like {
            return false;
        }
        if !matches!(next_stmt, Some(NextEmittedStatement::PushOnTarget)) {
            return false;
        }
        // Keep this narrow: only direct identifier assignments.
        let result = is_simple_identifier(&name);
        if debug {
            eprintln!(
                "[STRUCT_GAP] idx={} decision: blank-after-assignment-push name={} rhs={} result={}",
                idx, name, rhs, result
            );
        }
        result
    }

    fn flush_pending_sequence_expr(
        output: &mut String,
        pending: &mut Option<PendingSequenceExpr>,
        last_source_end_line: &mut Option<u32>,
    ) {
        if let Some(pending_expr) = pending.take() {
            for expr in pending_expr.exprs {
                append_statement_with_source_gap(
                    output,
                    &format!("{};\n", expr),
                    Some(&pending_expr.loc),
                    last_source_end_line,
                );
            }
        }
    }

    fn is_assignment_like_sequence_expr(expr: &str) -> bool {
        let trimmed = expr.trim();
        if trimmed.starts_with("let ") || trimmed.starts_with("const ") {
            return false;
        }
        trimmed.contains(" = ")
    }

    fn wrap_sequence_expr_item(expr: &str) -> String {
        let trimmed = expr.trim();
        if is_assignment_like_sequence_expr(trimmed)
            && !(trimmed.starts_with('(') && trimmed.ends_with(')'))
        {
            format!("({trimmed})")
        } else {
            trimmed.to_string()
        }
    }

    fn render_pending_sequence_prefix(pending: &PendingSequenceExpr) -> Option<String> {
        if pending.exprs.is_empty() {
            return None;
        }
        let mut rendered = Vec::with_capacity(pending.exprs.len());
        for expr in &pending.exprs {
            rendered.push(wrap_sequence_expr_item(expr));
        }
        Some(rendered.join(", "))
    }

    fn extract_simple_expression_statement(stmt: &str) -> Option<String> {
        let trimmed = stmt.trim();
        if trimmed.is_empty() || !trimmed.ends_with(';') || trimmed.contains('\n') {
            return None;
        }
        let expr = trimmed.trim_end_matches(';').trim();
        if expr.is_empty() {
            return None;
        }
        if expr.starts_with("let ")
            || expr.starts_with("const ")
            || expr.starts_with("if ")
            || expr.starts_with("while ")
            || expr.starts_with("for ")
            || expr.starts_with("do ")
            || expr.starts_with("switch ")
            || expr.starts_with("return ")
            || expr.starts_with("throw ")
            || expr.starts_with("try ")
            || expr.starts_with("break ")
            || expr.starts_with("continue ")
        {
            return None;
        }
        Some(expr.to_string())
    }

    fn source_locs_are_sequence_adjacent(
        prefix: &SourceLocation,
        current: &SourceLocation,
    ) -> bool {
        let (SourceLocation::Source(prefix), SourceLocation::Source(current)) = (prefix, current)
        else {
            return false;
        };
        if prefix.end.line != current.start.line || current.start.column < prefix.end.column {
            return false;
        }
        current.start.column - prefix.end.column <= 3
    }

    fn source_locs_are_sequence_compatible(
        prefix: &SourceLocation,
        current: &SourceLocation,
    ) -> bool {
        if source_locs_are_sequence_adjacent(prefix, current) {
            return true;
        }
        let (SourceLocation::Source(prefix), SourceLocation::Source(current)) = (prefix, current)
        else {
            return false;
        };
        prefix.start.line == current.start.line
            && current.start.column <= prefix.start.column
            && current.end.column >= prefix.end.column
    }

    fn combine_assignment_statement_with_sequence_prefix(
        stmt: &str,
        prefix_expr: &str,
    ) -> Option<String> {
        let trimmed = stmt.trim();
        if trimmed.is_empty() || !trimmed.ends_with(';') || trimmed.contains('\n') {
            return None;
        }
        let body = trimmed.trim_end_matches(';').trim();
        let prefix_trimmed = prefix_expr.trim();
        let prefix_expr = if prefix_trimmed.contains('=')
            && !prefix_trimmed.contains(',')
            && !(prefix_trimmed.starts_with('(') && prefix_trimmed.ends_with(')'))
        {
            format!("({prefix_expr})")
        } else {
            prefix_trimmed.to_string()
        };
        if let Some(rest) = body.strip_prefix("let ")
            && let Some((lhs, rhs)) = rest.split_once(" = ")
        {
            return Some(format!(
                "let {} = ({}, {});\n",
                lhs.trim(),
                prefix_expr.as_str(),
                rhs.trim()
            ));
        }
        if let Some(rest) = body.strip_prefix("const ")
            && let Some((lhs, rhs)) = rest.split_once(" = ")
        {
            return Some(format!(
                "const {} = ({}, {});\n",
                lhs.trim(),
                prefix_expr.as_str(),
                rhs.trim()
            ));
        }
        if let Some((lhs, rhs)) = body.split_once(" = ") {
            return Some(format!(
                "{} = ({}, {});\n",
                lhs.trim(),
                prefix_expr.as_str(),
                rhs.trim()
            ));
        }
        None
    }

    fn maybe_combine_instruction_with_pending_sequence(
        cx: &mut Context,
        instr: &ReactiveInstruction,
        stmt: &str,
        pending: &PendingSequenceExpr,
    ) -> Option<String> {
        let prefix_expr = render_pending_sequence_prefix(pending)?;
        match &instr.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                let lvalue = instr.lvalue.as_ref()?;
                if lvalue.identifier.name.is_none()
                    || place.identifier.name.is_some()
                    || !source_locs_are_sequence_adjacent(&pending.loc, &place.loc)
                {
                    return None;
                }
                combine_assignment_statement_with_sequence_prefix(stmt, &prefix_expr)
            }
            InstructionValue::StoreLocal { value, .. }
            | InstructionValue::StoreContext { value, .. } => {
                if instr.lvalue.is_some()
                    || value.identifier.name.is_some()
                    || !source_locs_are_sequence_compatible(&pending.loc, &value.loc)
                {
                    return None;
                }
                combine_assignment_statement_with_sequence_prefix(stmt, &prefix_expr)
            }
            InstructionValue::LogicalExpression {
                operator,
                left,
                right,
                ..
            } => {
                if instr.lvalue.is_some()
                    || !source_locs_are_sequence_compatible(&pending.loc, &left.loc)
                {
                    return None;
                }
                let logical_prec = logical_operator_precedence(operator);
                let left_expr = codegen_logical_operand(cx, left, logical_prec);
                let right_expr = codegen_logical_operand(cx, right, logical_prec);
                let left_from_prefix = pending.exprs.last().and_then(|expr| {
                    expr.split_once(" = ")
                        .map(|(_, rhs)| normalize_fusion_match_text(rhs))
                });
                let normalized_left_expr = normalize_fusion_match_text(&left_expr);
                let combined_left = if left_from_prefix
                    .as_ref()
                    .is_some_and(|rhs| rhs == &normalized_left_expr)
                {
                    if pending.exprs.len() == 1 {
                        prefix_expr
                    } else {
                        format!("({prefix_expr})")
                    }
                } else {
                    format!("({prefix_expr}, {left_expr})")
                };
                render_reactive_expression_statement_ast(&format!(
                    "{} {} {}",
                    combined_left,
                    logical_operator_to_str(operator),
                    right_expr
                ))
            }
            _ => None,
        }
    }

    fn maybe_codegen_while_with_pending_sequence(
        cx: &mut Context,
        terminal: &ReactiveTerminal,
        pending: &PendingSequenceExpr,
    ) -> Option<String> {
        let ReactiveTerminal::While {
            test, loop_block, ..
        } = terminal
        else {
            return None;
        };
        if test.identifier.name.is_some()
            || !source_locs_are_sequence_compatible(&pending.loc, &test.loc)
        {
            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                eprintln!(
                    "[CODEGEN_EXPR] skip-while-sequence pending_loc={:?} test_loc={:?} test_name={:?}",
                    pending.loc, test.loc, test.identifier.name
                );
            }
            return None;
        }
        let test_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Assignment);
        let pending_expr = render_pending_sequence_prefix(pending)?;
        if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
            eprintln!(
                "[CODEGEN_EXPR] fuse-while-sequence pending={} test_expr={}",
                pending_expr, test_expr
            );
        }
        let body = codegen_block(cx, loop_block);
        render_reactive_while_statement_ast(&format!("({}, {})", pending_expr, test_expr), &body)
    }

    fn sequence_seed_loc(instr: &ReactiveInstruction) -> &SourceLocation {
        match &instr.value {
            InstructionValue::StoreLocal { value, .. }
            | InstructionValue::StoreContext { value, .. } => &value.loc,
            _ => &instr.loc,
        }
    }

    fn is_reassign_store_without_outer_lvalue(instr: &ReactiveInstruction) -> bool {
        if instr.lvalue.is_some() {
            return false;
        }
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } => {
                lvalue.kind == InstructionKind::Reassign && lvalue.place.identifier.name.is_some()
            }
            _ => false,
        }
    }

    fn instruction_source_loc_for_spacing<'a>(
        cx: &'a Context,
        instr: &'a ReactiveInstruction,
    ) -> &'a SourceLocation {
        match &instr.value {
            InstructionValue::StoreLocal { value, .. }
            | InstructionValue::StoreContext { value, .. } => {
                if let Some(Some(expr_value)) = cx.temp.get(&value.identifier.declaration_id) {
                    let expr = expr_value.expr.trim_start();
                    if expr.starts_with("function ")
                        || expr.starts_with("async function ")
                        || expr.contains("=>")
                    {
                        return &instr.loc;
                    }
                }
                &value.loc
            }
            _ => &instr.loc,
        }
    }

    fn is_sequence_chainable_loc(seed: &SourceLocation, current: &SourceLocation) -> bool {
        let (SourceLocation::Source(seed), SourceLocation::Source(current)) = (seed, current)
        else {
            return false;
        };
        seed.start.line == current.start.line
            && (current.start.column >= seed.start.column
                || source_locs_are_sequence_compatible(
                    &SourceLocation::Source(seed.clone()),
                    &SourceLocation::Source(current.clone()),
                ))
    }

    fn instruction_extends_pending_sequence(
        instr: &ReactiveInstruction,
        pending: &PendingSequenceExpr,
    ) -> bool {
        if instr.lvalue.is_some() {
            return false;
        }
        match &instr.value {
            InstructionValue::StoreLocal { value, .. }
            | InstructionValue::StoreContext { value, .. } => {
                is_reassign_store_without_outer_lvalue(instr)
                    && value.identifier.name.is_none()
                    && is_sequence_chainable_loc(&pending.loc, &value.loc)
            }
            InstructionValue::PostfixUpdate { .. } | InstructionValue::PrefixUpdate { .. } => {
                is_sequence_chainable_loc(&pending.loc, &instr.loc)
            }
            _ => false,
        }
    }

    fn has_following_sequence_tail_assignment_pattern(
        block: &[ReactiveStatement],
        start: usize,
    ) -> bool {
        let ReactiveStatement::Instruction(start_instr) = &block[start] else {
            return false;
        };
        if !is_reassign_store_without_outer_lvalue(start_instr) {
            return false;
        }
        let seed_loc = sequence_seed_loc(start_instr);
        let mut saw_intermediate = false;
        let end = (start + 24).min(block.len());
        for stmt in block.iter().take(end).skip(start + 1) {
            let ReactiveStatement::Instruction(instr) = stmt else {
                break;
            };
            if instr.lvalue.is_some() {
                continue;
            }
            let is_candidate = matches!(
                instr.value,
                InstructionValue::PostfixUpdate { .. }
                    | InstructionValue::PrefixUpdate { .. }
                    | InstructionValue::StoreLocal { .. }
                    | InstructionValue::StoreContext { .. }
            );
            if !is_candidate {
                break;
            }
            match &instr.value {
                InstructionValue::StoreLocal { value, .. }
                | InstructionValue::StoreContext { value, .. } => {
                    if !is_reassign_store_without_outer_lvalue(instr)
                        || value.identifier.name.is_some()
                        || !is_sequence_chainable_loc(seed_loc, &value.loc)
                    {
                        break;
                    }
                    if saw_intermediate && source_locs_are_sequence_compatible(seed_loc, &value.loc)
                    {
                        return true;
                    }
                    saw_intermediate = true;
                }
                InstructionValue::PostfixUpdate { .. } | InstructionValue::PrefixUpdate { .. } => {
                    if !is_sequence_chainable_loc(seed_loc, &instr.loc) {
                        break;
                    }
                    saw_intermediate = true;
                }
                _ => break,
            }
        }
        false
    }

    fn single_call_expression_loc_in_block(block: &[ReactiveStatement]) -> Option<SourceLocation> {
        let mut call_loc: Option<SourceLocation> = None;
        for stmt in block {
            let ReactiveStatement::Instruction(instr) = stmt else {
                return None;
            };
            if instr.lvalue.is_none()
                && matches!(
                    instr.value,
                    InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                )
            {
                if call_loc.is_some() {
                    return None;
                }
                call_loc = Some(instr.loc.clone());
            }
        }
        call_loc
    }

    fn has_following_call_temp_load_pattern(block: &[ReactiveStatement], start: usize) -> bool {
        let mut call_temp_decl: Option<DeclarationId> = None;
        let end = (start + 8).min(block.len());
        for stmt in block.iter().take(end).skip(start + 1) {
            let ReactiveStatement::Instruction(instr) = stmt else {
                break;
            };
            if let Some(call_decl) = call_temp_decl {
                match &instr.value {
                    InstructionValue::LoadLocal { place, .. }
                    | InstructionValue::LoadContext { place, .. } => {
                        if place.identifier.declaration_id == call_decl
                            && let Some(lvalue) = &instr.lvalue
                            && lvalue.identifier.name.is_some()
                        {
                            return true;
                        }
                    }
                    _ => {}
                }
                if matches!(
                    instr.value,
                    InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
                ) {
                    break;
                }
                continue;
            }
            if matches!(
                instr.value,
                InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
            ) {
                let Some(lvalue) = &instr.lvalue else {
                    return false;
                };
                if lvalue.identifier.name.is_some() {
                    return false;
                }
                call_temp_decl = Some(lvalue.identifier.declaration_id);
            }
        }
        false
    }

    fn has_following_sequence_combine_target(
        block: &[ReactiveStatement],
        start: usize,
        seed_loc: &SourceLocation,
    ) -> bool {
        let end = (start + 8).min(block.len());
        for stmt in block.iter().take(end).skip(start + 1) {
            let ReactiveStatement::Instruction(instr) = stmt else {
                break;
            };
            match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    if let Some(lvalue) = &instr.lvalue {
                        if lvalue.identifier.name.is_some() {
                            return place.identifier.name.is_none()
                                && source_locs_are_sequence_adjacent(seed_loc, &place.loc);
                        }
                        continue;
                    }
                    break;
                }
                InstructionValue::StoreLocal { value, .. }
                | InstructionValue::StoreContext { value, .. } => {
                    return instr.lvalue.is_none()
                        && value.identifier.name.is_none()
                        && source_locs_are_sequence_compatible(seed_loc, &value.loc);
                }
                InstructionValue::LogicalExpression { left, .. } => {
                    return instr.lvalue.is_none()
                        && source_locs_are_sequence_compatible(seed_loc, &left.loc);
                }
                InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. } => {
                    return false;
                }
                _ => {
                    if instr
                        .lvalue
                        .as_ref()
                        .is_some_and(|lvalue| lvalue.identifier.name.is_none())
                    {
                        continue;
                    }
                    return false;
                }
            }
        }
        false
    }

    fn has_following_while_test_sequence_pattern(
        block: &[ReactiveStatement],
        start: usize,
        seed_loc: &SourceLocation,
    ) -> bool {
        let end = (start + 12).min(block.len());
        for stmt in block.iter().take(end).skip(start + 1) {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    // Any named write means we're no longer in an expression-only chain.
                    if instr
                        .lvalue
                        .as_ref()
                        .and_then(|lvalue| lvalue.identifier.name.as_ref())
                        .is_some()
                    {
                        return false;
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    if let ReactiveTerminal::While { test, .. } = &term_stmt.terminal {
                        return test.identifier.name.is_none()
                            && source_locs_are_sequence_compatible(seed_loc, &test.loc);
                    }
                    return false;
                }
                ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_) => return false,
            }
        }
        false
    }

    fn maybe_emit_unused_temp_named_load_statement(
        cx: &mut Context,
        block: &[ReactiveStatement],
        idx: usize,
        instr: &ReactiveInstruction,
    ) -> Option<String> {
        let lvalue = instr.lvalue.as_ref()?;
        if lvalue.identifier.name.is_some() {
            return None;
        }
        let place = match &instr.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => place,
            _ => return None,
        };
        if place.identifier.name.is_none()
            || !cx
                .reassigned_decls
                .contains(&place.identifier.declaration_id)
        {
            return None;
        }
        if reactive_block_uses_declaration(&block[idx + 1..], lvalue.identifier.declaration_id) {
            return None;
        }
        let expr = codegen_place_to_expression(cx, place);
        render_reactive_expression_statement_ast(&expr)
    }

    fn maybe_codegen_store_context_with_value_tail(
        cx: &mut Context,
        block: &[ReactiveStatement],
        idx: usize,
        instr: &ReactiveInstruction,
    ) -> Option<String> {
        if instr.lvalue.is_some() {
            return None;
        }
        let InstructionValue::StoreContext { lvalue, value, .. } = &instr.value else {
            return None;
        };
        if lvalue.kind != InstructionKind::Reassign || lvalue.place.identifier.name.is_none() {
            return None;
        }
        let target = identifier_name_with_cx(cx, &lvalue.place.identifier);
        let rhs_expr = codegen_place_to_expression(cx, value);
        if !contains_base_identifier_token(&rhs_expr, &target) || !rhs_expr.contains("||") {
            return None;
        }
        let ReactiveStatement::Instruction(next_instr) = block.get(idx + 1)? else {
            return None;
        };
        let Some(next_lvalue) = &next_instr.lvalue else {
            return None;
        };
        if next_lvalue.identifier.name.is_some() {
            return None;
        }
        let InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            ..
        } = &next_instr.value
        else {
            return None;
        };
        let mut stmt = codegen_instruction_nullable(cx, instr)?;
        stmt.push_str(&render_reactive_expression_statement_ast(&target)?);
        Some(stmt)
    }

    let mut output = String::new();
    let mut last_source_end_line: Option<u32> = None;
    let mut pending_sequence_expr: Option<PendingSequenceExpr> = None;
    let mut i = 0usize;
    let debug_scope_bridge = std::env::var("DEBUG_SCOPE_BRIDGE").is_ok();
    let debug_codegen_trace = std::env::var("DEBUG_CODEGEN_TRACE").is_ok();
    while i < block.len() {
        if let Some(consumed) =
            maybe_codegen_fused_pruned_scope_prefix_into_following_stmt(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }

        if let Some(consumed) =
            maybe_codegen_fused_method_call_eval_order(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }

        if let Some(consumed) =
            maybe_codegen_fused_method_call_destructure_assignment(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) = maybe_codegen_fused_nullish_self_reassign(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) = maybe_codegen_fused_named_test_dual_reassign_scope_ternary_return(
            cx,
            block,
            i,
            &mut output,
        ) {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_named_test_scope_decl_ternary_statement(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_named_test_reassign_then_ternary_branch(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_reassign_then_ternary_branch(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_reassign_temp_load_then_ternary(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_reassign_stmt_into_following_null(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_reassign_stmt_into_following_logical(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_temp_load_into_following_stmt(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_named_temp_ternary_statement(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }
        if let Some(consumed) =
            maybe_codegen_fused_named_temp_logical_expression(cx, block, i, &mut output)
        {
            last_source_end_line = None;
            i += consumed;
            continue;
        }

        if debug_scope_bridge && let ReactiveStatement::Terminal(term_stmt) = &block[i] {
            let label_desc = match &term_stmt.label {
                None => "none".to_string(),
                Some(label) => format!("some(implicit={})", label.implicit),
            };
            let label_ok = term_stmt
                .label
                .as_ref()
                .map(|label| label.implicit)
                .unwrap_or(true);
            let terminal_loc = match &term_stmt.terminal {
                ReactiveTerminal::If { loc, .. }
                | ReactiveTerminal::Switch { loc, .. }
                | ReactiveTerminal::DoWhile { loc, .. }
                | ReactiveTerminal::While { loc, .. }
                | ReactiveTerminal::For { loc, .. }
                | ReactiveTerminal::ForOf { loc, .. }
                | ReactiveTerminal::ForIn { loc, .. }
                | ReactiveTerminal::Try { loc, .. }
                | ReactiveTerminal::Label { loc, .. }
                | ReactiveTerminal::Break { loc, .. }
                | ReactiveTerminal::Continue { loc, .. }
                | ReactiveTerminal::Return { loc, .. }
                | ReactiveTerminal::Throw { loc, .. } => match loc {
                    SourceLocation::Source(range) => {
                        format!(
                            "{}:{}-{}:{}",
                            range.start.line, range.start.column, range.end.line, range.end.column
                        )
                    }
                    SourceLocation::Generated => "generated".to_string(),
                },
            };
            eprintln!(
                "[SCOPE_BRIDGE] block_idx={} label={} label_ok={} next_is_scope={} terminal_loc={}",
                i,
                label_desc,
                label_ok,
                matches!(block.get(i + 1), Some(ReactiveStatement::Scope(_))),
                terminal_loc
            );
            for (lookahead, stmt) in block
                .iter()
                .enumerate()
                .take((i + 6).min(block.len()))
                .skip(i + 1)
            {
                let desc = match stmt {
                    ReactiveStatement::Instruction(instr) => {
                        let value_kind = match &instr.value {
                            InstructionValue::DeclareLocal { .. } => "DeclareLocal",
                            InstructionValue::DeclareContext { .. } => "DeclareContext",
                            InstructionValue::StoreLocal { .. } => "StoreLocal",
                            InstructionValue::StoreContext { .. } => "StoreContext",
                            InstructionValue::StartMemoize { .. } => "StartMemoize",
                            InstructionValue::FinishMemoize { .. } => "FinishMemoize",
                            _ => "InstructionOther",
                        };
                        format!("Instruction({value_kind}) value={:?}", instr.value)
                    }
                    ReactiveStatement::Scope(_) => "Scope".to_string(),
                    ReactiveStatement::PrunedScope(_) => "PrunedScope".to_string(),
                    ReactiveStatement::Terminal(_) => "Terminal".to_string(),
                };
                eprintln!("[SCOPE_BRIDGE]   lookahead idx={} {}", lookahead, desc);
            }
        }
        // Synthesize a cache guard for control-flow merged reassignments when
        // the next scope depends on that reassigned variable.
        if std::env::var("ENABLE_SCOPE_BRIDGE").is_ok()
            && !cx.disable_memoization_features
            && let ReactiveStatement::Terminal(term_stmt) = &block[i]
            && term_stmt
                .label
                .as_ref()
                .map(|label| label.implicit)
                .unwrap_or(true)
            && let Some((test_place, target_ident)) =
                memoizable_if_reassignment_scope_bridge(&term_stmt.terminal, &block[i + 1..])
            && let Some(if_stmt) = codegen_terminal(cx, &term_stmt.terminal)
        {
            let cache_var = cx.synthesize_name("$");
            let dep_expr = codegen_place_to_expression(cx, test_place);
            let dep_slot = cx.alloc_cache_slot();
            let value_slot = cx.alloc_cache_slot();
            let target_name = identifier_name_with_cx(cx, &target_ident);
            let mut consequent = if_stmt;
            consequent.push_str(
                &render_reactive_expression_statement_ast(&format!(
                    "{}[{}] = {}",
                    cache_var, dep_slot, dep_expr
                ))
                .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, dep_slot, dep_expr)),
            );
            consequent.push_str(
                &render_reactive_expression_statement_ast(&format!(
                    "{}[{}] = {}",
                    cache_var, value_slot, target_name
                ))
                .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, value_slot, target_name)),
            );
            let alternate = render_reactive_assignment_statement_ast(
                &target_name,
                &format!("{}[{}]", cache_var, value_slot),
            )
            .unwrap_or_else(|| format!("{} = {}[{}];\n", target_name, cache_var, value_slot));
            let guard_test = format!("{}[{}] !== {}", cache_var, dep_slot, dep_expr);
            output.push_str(
                &render_reactive_if_statement_ast(&guard_test, &consequent, Some(&alternate))
                    .unwrap_or_else(|| {
                        format!(
                            "if ({}) {{\n{}}} else {{\n{}}}\n",
                            guard_test, consequent, alternate
                        )
                    }),
            );
            last_source_end_line = None;
            i += 1;
            continue;
        }

        let item = &block[i];
        match item {
            ReactiveStatement::Instruction(instr) => {
                if debug_codegen_trace {
                    let lvalue_decl = instr
                        .lvalue
                        .as_ref()
                        .map(|lvalue| lvalue.identifier.declaration_id.0);
                    let lvalue_name = instr
                        .lvalue
                        .as_ref()
                        .and_then(|lvalue| lvalue.identifier.name.as_ref())
                        .map(IdentifierName::value)
                        .unwrap_or("<none>");
                    eprintln!(
                        "[CODEGEN_TRACE] idx={} instr#{} kind={} lvalue_decl={:?} lvalue_name={}",
                        i,
                        instr.id.0,
                        instruction_value_tag(&instr.value),
                        lvalue_decl,
                        lvalue_name
                    );
                }
                if maybe_defer_inlineable_ternary_into_following_scope(
                    cx,
                    instr,
                    &block[..i],
                    &block[i + 1..],
                ) {
                    if debug_codegen_trace {
                        eprintln!(
                            "[CODEGEN_TRACE] idx={} instr#{} emitted=<deferred-inline-ternary>",
                            i, instr.id.0
                        );
                    }
                    i += 1;
                    continue;
                }
                if let Some(stmt) = maybe_emit_unused_temp_named_load_statement(cx, block, i, instr)
                {
                    let spacing_loc = instruction_source_loc_for_spacing(cx, instr).clone();
                    flush_pending_sequence_expr(
                        &mut output,
                        &mut pending_sequence_expr,
                        &mut last_source_end_line,
                    );
                    append_statement_with_source_gap(
                        &mut output,
                        &stmt,
                        Some(&spacing_loc),
                        &mut last_source_end_line,
                    );
                    if should_insert_structural_blank_after_instruction(
                        cx,
                        block,
                        i,
                        &stmt,
                        Some(&spacing_loc),
                    ) && !output.ends_with("\n\n")
                    {
                        output.push('\n');
                    }
                    i += 1;
                    continue;
                }
                if let Some(stmt) = maybe_codegen_store_context_with_value_tail(cx, block, i, instr)
                {
                    let spacing_loc = instruction_source_loc_for_spacing(cx, instr).clone();
                    flush_pending_sequence_expr(
                        &mut output,
                        &mut pending_sequence_expr,
                        &mut last_source_end_line,
                    );
                    append_statement_with_source_gap(
                        &mut output,
                        &stmt,
                        Some(&spacing_loc),
                        &mut last_source_end_line,
                    );
                    if should_insert_structural_blank_after_instruction(
                        cx,
                        block,
                        i,
                        &stmt,
                        Some(&spacing_loc),
                    ) && !output.ends_with("\n\n")
                    {
                        output.push('\n');
                    }
                    i += 1;
                    continue;
                }
                if let Some(pending) = &pending_sequence_expr
                    && let Some(stmt) = codegen_instruction_nullable(cx, instr)
                {
                    if debug_codegen_trace {
                        eprintln!(
                            "[CODEGEN_TRACE] idx={} instr#{} emitted={:?}",
                            i, instr.id.0, stmt
                        );
                    }
                    if let Some(combined) =
                        maybe_combine_instruction_with_pending_sequence(cx, instr, &stmt, pending)
                        && !has_following_sequence_tail_assignment_pattern(block, i)
                    {
                        let spacing_loc = instruction_source_loc_for_spacing(cx, instr).clone();
                        append_statement_with_source_gap(
                            &mut output,
                            &combined,
                            Some(&spacing_loc),
                            &mut last_source_end_line,
                        );
                        if should_insert_structural_blank_after_instruction(
                            cx,
                            block,
                            i,
                            &combined,
                            Some(&spacing_loc),
                        ) && !output.ends_with("\n\n")
                        {
                            output.push('\n');
                        }
                        pending_sequence_expr = None;
                        i += 1;
                        continue;
                    }
                    if let Some(expr) = extract_simple_expression_statement(&stmt)
                        && instruction_extends_pending_sequence(instr, pending)
                    {
                        if let Some(pending_mut) = pending_sequence_expr.as_mut() {
                            pending_mut.exprs.push(expr);
                        }
                        i += 1;
                        continue;
                    }
                    flush_pending_sequence_expr(
                        &mut output,
                        &mut pending_sequence_expr,
                        &mut last_source_end_line,
                    );
                    let spacing_loc = instruction_source_loc_for_spacing(cx, instr).clone();
                    append_statement_with_source_gap(
                        &mut output,
                        &stmt,
                        Some(&spacing_loc),
                        &mut last_source_end_line,
                    );
                    if should_insert_structural_blank_after_instruction(
                        cx,
                        block,
                        i,
                        &stmt,
                        Some(&spacing_loc),
                    ) && !output.ends_with("\n\n")
                    {
                        output.push('\n');
                    }
                    i += 1;
                    continue;
                }

                if let Some(stmt) = codegen_instruction_nullable(cx, instr) {
                    if debug_codegen_trace {
                        eprintln!(
                            "[CODEGEN_TRACE] idx={} instr#{} emitted={:?}",
                            i, instr.id.0, stmt
                        );
                    }
                    let starts_from_reassign_store = instr.lvalue.is_none()
                        && matches!(
                            instr.value,
                            InstructionValue::StoreLocal {
                                lvalue: LValue {
                                    kind: InstructionKind::Reassign,
                                    ..
                                },
                                ..
                            } | InstructionValue::StoreContext {
                                lvalue: LValue {
                                    kind: InstructionKind::Reassign,
                                    ..
                                },
                                ..
                            }
                        );
                    let starts_from_side_effect_call = instr.lvalue.is_none()
                        && matches!(
                            instr.value,
                            InstructionValue::CallExpression { .. }
                                | InstructionValue::MethodCall { .. }
                        );
                    let seed_loc = sequence_seed_loc(instr);
                    let has_sequence_tail =
                        has_following_sequence_tail_assignment_pattern(block, i);
                    let has_call_temp_load = has_following_call_temp_load_pattern(block, i);
                    let has_sequence_combine_target =
                        has_following_sequence_combine_target(block, i, seed_loc);
                    let has_while_test_tail =
                        has_following_while_test_sequence_pattern(block, i, seed_loc);
                    if (starts_from_reassign_store || starts_from_side_effect_call)
                        && (has_call_temp_load
                            || has_sequence_tail
                            || has_sequence_combine_target
                            || has_while_test_tail)
                        && let Some(expr) = extract_simple_expression_statement(&stmt)
                    {
                        flush_pending_sequence_expr(
                            &mut output,
                            &mut pending_sequence_expr,
                            &mut last_source_end_line,
                        );
                        pending_sequence_expr = Some(PendingSequenceExpr {
                            exprs: vec![expr],
                            loc: seed_loc.clone(),
                        });
                        i += 1;
                        continue;
                    }
                    flush_pending_sequence_expr(
                        &mut output,
                        &mut pending_sequence_expr,
                        &mut last_source_end_line,
                    );
                    let spacing_loc = instruction_source_loc_for_spacing(cx, instr).clone();
                    append_statement_with_source_gap(
                        &mut output,
                        &stmt,
                        Some(&spacing_loc),
                        &mut last_source_end_line,
                    );
                    if should_insert_structural_blank_after_instruction(
                        cx,
                        block,
                        i,
                        &stmt,
                        Some(&spacing_loc),
                    ) && !output.ends_with("\n\n")
                    {
                        output.push('\n');
                    }
                } else if debug_codegen_trace {
                    eprintln!(
                        "[CODEGEN_TRACE] idx={} instr#{} emitted=<none>",
                        i, instr.id.0
                    );
                }
            }
            ReactiveStatement::PrunedScope(pruned) => {
                flush_pending_sequence_expr(
                    &mut output,
                    &mut pending_sequence_expr,
                    &mut last_source_end_line,
                );
                // Pruned scopes: emit instructions without memoization
                let inner = codegen_block_no_reset(cx, &pruned.instructions);
                if let Some(expr) = extract_simple_expression_statement(&inner)
                    && let Some(loc) = single_call_expression_loc_in_block(&pruned.instructions)
                {
                    pending_sequence_expr = Some(PendingSequenceExpr {
                        exprs: vec![expr],
                        loc,
                    });
                } else {
                    output.push_str(&inner);
                    last_source_end_line = None;
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                flush_pending_sequence_expr(
                    &mut output,
                    &mut pending_sequence_expr,
                    &mut last_source_end_line,
                );
                maybe_emit_reused_optional_dependency_reads(
                    cx,
                    &mut output,
                    &scope_block.scope,
                    &block[i + 1..],
                );
                if let Some(scope_dependencies) =
                    cx.scope_dependency_overrides.remove(&scope_block.scope.id)
                {
                    let mut overridden_scope = scope_block.scope.clone();
                    overridden_scope.dependencies = scope_dependencies;
                    codegen_reactive_scope(
                        cx,
                        &mut output,
                        &overridden_scope,
                        &scope_block.instructions,
                    );
                    last_source_end_line = None;
                    i += 1;
                    continue;
                }
                if cx.disable_memoization_features {
                    let inner = codegen_block_no_reset(cx, &scope_block.instructions);
                    output.push_str(&inner);
                    last_source_end_line = None;
                    i += 1;
                    continue;
                }
                if should_inline_zero_dep_global_zero_arg_call_scope(
                    cx,
                    scope_block,
                    &block[..i],
                    &block[i + 1..],
                ) {
                    let inner = codegen_block_no_reset(cx, &scope_block.instructions);
                    output.push_str(&inner);
                    last_source_end_line = None;
                    i += 1;
                    continue;
                }
                if allow_top_level_zero_dep_literal_inline
                    && let Some(consumed_following) = maybe_codegen_inline_literal_init_scope(
                        cx,
                        scope_block,
                        &block[i + 1..],
                        &mut output,
                    )
                {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) = maybe_codegen_fused_ternary_source_scope(
                    cx,
                    scope_block,
                    &block[i + 1..],
                    &mut output,
                ) {
                    // Consumed this scope and the matched following statements.
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) =
                    maybe_codegen_inline_zero_dep_literal_into_following_scope(
                        cx,
                        scope_block,
                        &block[i + 1..],
                        &mut output,
                    )
                {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) =
                    maybe_codegen_fused_dual_zero_dep_literal_ternary_scope(
                        cx,
                        scope_block,
                        &block[i + 1..],
                        &mut output,
                    )
                {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) = maybe_codegen_fused_zero_dep_literal_store_scope(
                    cx,
                    scope_block,
                    &block[i + 1..],
                    &mut output,
                ) {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                // Upstream does not fuse a zero-dep seed scope directly into a following
                // dep-guarded scope; it emits distinct memoization guards/slots for each scope.
                // Keep this path disabled for strict output parity.
                if let Some(consumed_following) =
                    maybe_codegen_fused_zero_dep_sequence_logical_scope(
                        cx,
                        scope_block,
                        &block[i + 1..],
                        &mut output,
                    )
                {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) = maybe_codegen_fused_zero_dep_ternary_default_scope(
                    cx,
                    scope_block,
                    &block[i + 1..],
                    &mut output,
                ) {
                    // Consumed this scope and the matched following statements.
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) = maybe_codegen_fused_callback_reassign_scope(
                    cx,
                    scope_block,
                    &block[i + 1..],
                    &mut output,
                ) {
                    // Consumed this scope and the matched following statements.
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(consumed_following) =
                    maybe_codegen_fused_effect_callback_empty_array_scope(
                        cx,
                        scope_block,
                        &block[i + 1..],
                        &mut output,
                    )
                {
                    last_source_end_line = None;
                    i += 1 + consumed_following;
                    continue;
                }
                if let Some(filtered_deps) = filtered_effect_callback_deps_for_explicit_empty_array(
                    cx,
                    scope_block,
                    &block[i + 1..],
                ) {
                    let mut overridden_scope = scope_block.scope.clone();
                    overridden_scope.dependencies = filtered_deps;
                    codegen_reactive_scope(
                        cx,
                        &mut output,
                        &overridden_scope,
                        &scope_block.instructions,
                    );
                    last_source_end_line = None;
                    i += 1;
                    continue;
                }
                let temp_snapshot = cx.snapshot_temps();
                // Bridge a following return by synthesizing an output when this scope
                // has no explicit outputs (no declarations or reassignments).
                // This enables sentinel-based caching for zero-dep scopes that
                // compute values consumed by an immediate `return`.
                if scope_block.scope.declarations.is_empty()
                    && scope_block.scope.reassignments.is_empty()
                    && let Some(ReactiveStatement::Terminal(term_stmt)) = block.get(i + 1)
                    && let ReactiveTerminal::Return { value, .. } = &term_stmt.terminal
                {
                    // Clone the scope and treat the return value as a reassignment output,
                    // so sentinel-based caching uses it as the first output slot.
                    let mut scope_clone = scope_block.scope.clone();
                    // Avoid duplicates just in case.
                    if !scope_clone
                        .reassignments
                        .iter()
                        .any(|id| id.id == value.identifier.id)
                    {
                        scope_clone.reassignments.push(value.identifier.clone());
                    }
                    codegen_reactive_scope(
                        cx,
                        &mut output,
                        &scope_clone,
                        &scope_block.instructions,
                    );
                    last_source_end_line = None;
                    cx.restore_temps(temp_snapshot);
                    // Proceed to emit the following return as usual in the next loop iteration.
                    i += 1;
                    continue;
                }
                codegen_reactive_scope(
                    cx,
                    &mut output,
                    &scope_block.scope,
                    &scope_block.instructions,
                );
                last_source_end_line = None;
                cx.restore_temps(temp_snapshot);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                if let Some(pending) = &pending_sequence_expr
                    && let Some(stmt) =
                        maybe_codegen_while_with_pending_sequence(cx, &term_stmt.terminal, pending)
                {
                    if let Some(label) = &term_stmt.label {
                        if !label.implicit {
                            emit_labeled_statement(&mut output, label.id, &stmt);
                        } else {
                            output.push_str(&stmt);
                        }
                    } else {
                        output.push_str(&stmt);
                    }
                    pending_sequence_expr = None;
                    last_source_end_line = None;
                    if !output.ends_with('\n') {
                        output.push('\n');
                    }
                    i += 1;
                    continue;
                }
                flush_pending_sequence_expr(
                    &mut output,
                    &mut pending_sequence_expr,
                    &mut last_source_end_line,
                );
                if let Some((rewritten, consumed_following)) =
                    maybe_codegen_inverted_labeled_if_fallthrough(cx, term_stmt, block, i)
                {
                    output.push_str(&rewritten);
                    last_source_end_line = None;
                    if !output.ends_with('\n') {
                        output.push('\n');
                    }
                    i += consumed_following + 1;
                    continue;
                }
                if let Some(consumed_following) =
                    maybe_codegen_labeled_if_adjacent_block(cx, term_stmt, block, i, &mut output)
                {
                    last_source_end_line = None;
                    i += consumed_following + 1;
                    continue;
                }
                if let Some(stmt) = codegen_terminal(cx, &term_stmt.terminal) {
                    let insert_blank_after_labeled_if =
                        should_insert_blank_after_labeled_if(term_stmt, block, i);
                    let insert_blank_after_try =
                        should_insert_blank_after_try_call_followup(cx, term_stmt, block, i);
                    let insert_blank_after_breaking_if =
                        should_insert_blank_after_if_trailing_labeled_break(
                            cx, term_stmt, block, i,
                        );
                    if let Some(label) = &term_stmt.label {
                        if !label.implicit {
                            emit_labeled_statement(&mut output, label.id, &stmt);
                        } else {
                            output.push_str(&stmt);
                        }
                    } else {
                        output.push_str(&stmt);
                    }
                    last_source_end_line = None;
                    if !output.ends_with('\n') {
                        output.push('\n');
                    }
                    if (insert_blank_after_labeled_if
                        || insert_blank_after_try
                        || insert_blank_after_breaking_if)
                        && !output.ends_with("\n\n")
                    {
                        output.push('\n');
                    }
                }
            }
        }
        i += 1;
    }
    flush_pending_sequence_expr(
        &mut output,
        &mut pending_sequence_expr,
        &mut last_source_end_line,
    );
    output
}

fn split_trailing_break(block: &[ReactiveStatement]) -> Option<(BlockId, &[ReactiveStatement])> {
    if block.is_empty() {
        return None;
    }
    let last = block.last()?;
    let ReactiveStatement::Terminal(ReactiveTerminalStatement {
        terminal: ReactiveTerminal::Break { target, .. },
        ..
    }) = last
    else {
        return None;
    };
    Some((*target, &block[..block.len() - 1]))
}

fn maybe_codegen_inverted_labeled_if_fallthrough(
    cx: &mut Context,
    term_stmt: &ReactiveTerminalStatement,
    block: &[ReactiveStatement],
    idx: usize,
) -> Option<(String, usize)> {
    let debug = std::env::var("DEBUG_LABEL_FALLTHROUGH").is_ok();
    macro_rules! bail {
        ($reason:expr) => {{
            if debug {
                eprintln!("[LABEL_FALLTHROUGH] skip: {}", $reason);
            }
            return None;
        }};
    }

    let Some(label) = &term_stmt.label else {
        bail!("no-label");
    };
    if label.implicit {
        bail!("implicit-label");
    }

    let ReactiveTerminal::If {
        test,
        consequent,
        alternate,
        ..
    } = &term_stmt.terminal
    else {
        bail!("non-if-terminal");
    };
    if alternate.is_some() {
        bail!("has-alternate");
    }

    let Some((cons_target, cons_prefix)) = split_trailing_break(consequent) else {
        bail!("consequent-no-trailing-break");
    };
    // Only apply to outer-target break inversion patterns.
    if cons_target == label.id {
        bail!("consequent-target-is-self");
    }
    if cons_prefix.is_empty() || !cons_prefix.iter().all(is_instruction_statement) {
        bail!("consequent-not-instruction-only");
    }

    if idx + 1 >= block.len() {
        bail!("no-following");
    }
    let following = &block[idx + 1..];
    let mut consumed_following = None;
    for take in 1..=following.len() {
        let slice = &following[..take];
        if let Some((target, _)) = split_trailing_break(slice)
            && target == cons_target
        {
            consumed_following = Some(take);
            break;
        }
    }
    let Some(consumed_following) = consumed_following else {
        bail!("no-following-slice-with-matching-break");
    };
    let fallthrough_block = &following[..consumed_following];
    let Some((fallthrough_target, fallthrough_prefix)) = split_trailing_break(fallthrough_block)
    else {
        bail!("fallthrough-no-trailing-break");
    };
    if fallthrough_target != cons_target
        || fallthrough_prefix.is_empty()
        || !fallthrough_prefix.iter().all(is_instruction_statement)
    {
        bail!("fallthrough-shape-mismatch");
    }

    let test_expr = codegen_place_to_expression(cx, test);
    let fallthrough_code = codegen_block(cx, fallthrough_prefix);
    let consequent_code = codegen_block(cx, cons_prefix);
    // Only rewrite very simple one-statement branches.
    if count_non_empty_lines(&fallthrough_code) != 1 || count_non_empty_lines(&consequent_code) != 1
    {
        bail!("branch-codes-not-single-statement");
    }
    // Guard against over-eager rewrites (e.g. call expressions) that diverge
    // from upstream output shape in non-inversion fixtures.
    if !is_simple_assignment_line(&fallthrough_code) || !is_simple_assignment_line(&consequent_code)
    {
        bail!("branch-codes-not-simple-assignment");
    }
    if debug {
        eprintln!(
            "[LABEL_FALLTHROUGH] apply: label=bb{} implicit={} target=bb{} test={} fallthrough={:?} consequent={:?}",
            label.id.0, label.implicit, cons_target.0, test_expr, fallthrough_code, consequent_code
        );
    }

    let rewritten = format!(
        "bb{}: if ({}) {{\nbreak bb{};\n}}\n{}break bb{};\n{}",
        label.id.0, test_expr, label.id.0, fallthrough_code, cons_target.0, consequent_code
    );
    Some((rewritten, consumed_following))
}

fn is_instruction_statement(stmt: &ReactiveStatement) -> bool {
    matches!(stmt, ReactiveStatement::Instruction(_))
}

fn count_non_empty_lines(code: &str) -> usize {
    code.lines().filter(|line| !line.trim().is_empty()).count()
}

fn is_simple_assignment_line(code: &str) -> bool {
    let trimmed = code.trim();
    if trimmed.contains('\n') || !trimmed.ends_with(';') {
        return false;
    }
    let body = trimmed.trim_end_matches(';').trim();
    if body.starts_with("let ")
        || body.starts_with("const ")
        || body.starts_with("if ")
        || body.starts_with("switch ")
        || body.starts_with("for ")
        || body.starts_with("while ")
        || body.starts_with("return ")
        || body.starts_with("break ")
        || body.starts_with("continue ")
    {
        return false;
    }
    body.contains(" = ")
}

fn emit_labeled_statement(output: &mut String, label_id: BlockId, stmt: &str) {
    // If no control transfer references this label in rendered statement,
    // skip emitting it (parity with upstream label pruning pass).
    if !statement_references_label(stmt, label_id) {
        output.push_str(stmt);
        return;
    }

    let trimmed = stmt.trim_start();
    if trimmed.is_empty() {
        output.push_str(&format!("bb{}:\n", label_id.0));
        return;
    }
    let own_label_prefix = format!("bb{}:", label_id.0);
    if trimmed.starts_with(&own_label_prefix) {
        output.push_str(trimmed);
        if !trimmed.ends_with('\n') {
            output.push('\n');
        }
        return;
    }
    let label = format!("bb{}", label_id.0);
    let needs_block_wrapper = labeled_statement_needs_block_wrapper(trimmed);
    if let Some(rendered) =
        render_reactive_labeled_statement_ast(&label, trimmed, needs_block_wrapper)
    {
        output.push_str(&rendered);
    } else if needs_block_wrapper {
        output.push_str(&format!("bb{}: {{\n", label_id.0));
        for line in trimmed.lines() {
            if line.trim().is_empty() {
                output.push('\n');
            } else {
                output.push_str("  ");
                output.push_str(line);
                output.push('\n');
            }
        }
        output.push_str("}\n");
    } else {
        output.push_str(&format!("bb{}: {}", label_id.0, trimmed));
        if !trimmed.ends_with('\n') {
            output.push('\n');
        }
    }
}

fn statement_references_label(stmt: &str, label_id: BlockId) -> bool {
    let label = format!("bb{}", label_id.0);
    stmt.contains(&format!("break {label}")) || stmt.contains(&format!("continue {label}"))
}

fn maybe_codegen_labeled_if_adjacent_block(
    cx: &mut Context,
    term_stmt: &ReactiveTerminalStatement,
    block: &[ReactiveStatement],
    idx: usize,
    output: &mut String,
) -> Option<usize> {
    let debug = std::env::var("DEBUG_LABEL_ADJ").is_ok();
    macro_rules! bail {
        ($reason:expr) => {{
            if debug {
                eprintln!("[LABEL_ADJ] skip: {}", $reason);
            }
            return None;
        }};
    }

    let label = term_stmt.label.as_ref()?;
    if label.implicit {
        bail!("implicit-label");
    }
    let ReactiveTerminal::If {
        consequent,
        alternate,
        loc,
        ..
    } = &term_stmt.terminal
    else {
        bail!("non-if");
    };
    let SourceLocation::Source(range) = loc else {
        bail!("generated-loc");
    };
    let term_start_line = range.start.line;
    let term_end_line = range.end.line;

    let branch_breaks_to_label = |branch: &[ReactiveStatement]| {
        split_trailing_break(branch)
            .map(|(target, prefix)| target == label.id && prefix.is_empty())
            .unwrap_or(false)
    };
    let consequent_breaks_to_label = branch_breaks_to_label(consequent);
    let alternate_breaks_to_label = alternate
        .as_ref()
        .map(|branch| branch_breaks_to_label(branch))
        .unwrap_or(false);
    if !consequent_breaks_to_label && !alternate_breaks_to_label {
        bail!("no-self-break-branch");
    }

    // Collect the first concrete source statement after this terminal.
    let mut consumed_following = 0usize;
    let mut first_following_line = None;
    let mut following_stmt = String::new();
    while let Some(ReactiveStatement::Instruction(instr)) = block.get(idx + 1 + consumed_following)
    {
        let Some(line) = reactive_instruction_start_line(instr) else {
            break;
        };
        if let Some(first_line) = first_following_line {
            if line != first_line {
                break;
            }
        } else {
            first_following_line = Some(line);
        }
        if let Some(stmt) = codegen_instruction_nullable(cx, instr) {
            following_stmt.push_str(&stmt);
            if !stmt.ends_with('\n') {
                following_stmt.push('\n');
            }
        }
        consumed_following += 1;
    }
    let Some(following_line) = first_following_line else {
        bail!("no-following-instruction");
    };
    // When the next statement starts on the immediately following source line,
    // upstream keeps it inside the labeled block.
    if following_line != term_end_line + 1 {
        bail!(format!(
            "following-line-mismatch term_end={} follow={}",
            term_end_line, following_line
        ));
    }
    if following_stmt.trim().is_empty() {
        bail!("following-stmt-empty");
    }
    if following_stmt.trim_start().starts_with("$[") {
        bail!("following-is-cache-store");
    }

    let mut prefix_stmt = None;
    let allow_prefix_lift = consequent_breaks_to_label && alternate.is_none();
    if allow_prefix_lift
        && term_start_line > 1
        && idx > 0
        && let Some(ReactiveStatement::Instruction(prev_instr)) = block.get(idx - 1)
        && reactive_instruction_start_line(prev_instr)
            .is_some_and(|line| line == term_start_line || line + 1 == term_start_line)
    {
        prefix_stmt = pop_last_simple_statement_line(output);
        if debug {
            eprintln!(
                "[LABEL_ADJ] prefix-candidate idx={} prev_line={} term_start={} popped={}",
                idx,
                reactive_instruction_start_line(prev_instr).unwrap_or_default(),
                term_start_line,
                prefix_stmt.is_some()
            );
        }
    } else if debug {
        let prev_line = if idx > 0 {
            match block.get(idx - 1) {
                Some(ReactiveStatement::Instruction(prev_instr)) => {
                    reactive_instruction_start_line(prev_instr).unwrap_or_default()
                }
                _ => 0,
            }
        } else {
            0
        };
        eprintln!(
            "[LABEL_ADJ] prefix-skip idx={} prev_line={} term_start={} allow_prefix={}",
            idx, prev_line, term_start_line, allow_prefix_lift
        );
    }

    let Some(if_stmt) = codegen_terminal(cx, &term_stmt.terminal) else {
        bail!("codegen-terminal-none");
    };
    let mut rewritten = format!("bb{}: {{\n", label.id.0);

    let had_prefix_stmt = prefix_stmt.is_some();
    if let Some(prefix) = prefix_stmt {
        rewritten.push_str("  ");
        rewritten.push_str(prefix.trim());
        rewritten.push('\n');
        rewritten.push('\n');
    }

    for line in if_stmt.trim_end().lines() {
        if line.trim().is_empty() {
            rewritten.push('\n');
        } else {
            rewritten.push_str("  ");
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    if !had_prefix_stmt && !rewritten.ends_with("\n\n") {
        // Keep a spacer between `if (...)` and the following lifted statement.
        rewritten.push('\n');
    }
    for line in following_stmt.trim_end().lines() {
        if line.trim().is_empty() {
            rewritten.push('\n');
        } else {
            rewritten.push_str("  ");
            rewritten.push_str(line);
            rewritten.push('\n');
        }
    }
    rewritten.push_str("}\n");
    let next_is_cache_store =
        next_emitted_statement_is_cache_store(cx, block, idx + 1 + consumed_following);
    if matches!(next_is_cache_store, Some(false)) {
        rewritten.push('\n');
    }
    if debug {
        eprintln!(
            "[LABEL_ADJ] apply: label=bb{} consumed={} prefix={} term={}..{} next_cache={:?}",
            label.id.0,
            consumed_following,
            had_prefix_stmt,
            term_start_line,
            term_end_line,
            next_is_cache_store
        );
    }
    output.push_str(&rewritten);
    Some(consumed_following)
}

fn reactive_instruction_start_line(instr: &ReactiveInstruction) -> Option<u32> {
    match &instr.loc {
        SourceLocation::Source(range) => Some(range.start.line),
        SourceLocation::Generated => None,
    }
}

fn reactive_terminal_start_line(terminal: &ReactiveTerminal) -> Option<u32> {
    let loc = match terminal {
        ReactiveTerminal::Break { loc, .. }
        | ReactiveTerminal::Continue { loc, .. }
        | ReactiveTerminal::Return { loc, .. }
        | ReactiveTerminal::Throw { loc, .. }
        | ReactiveTerminal::Switch { loc, .. }
        | ReactiveTerminal::DoWhile { loc, .. }
        | ReactiveTerminal::While { loc, .. }
        | ReactiveTerminal::For { loc, .. }
        | ReactiveTerminal::ForOf { loc, .. }
        | ReactiveTerminal::ForIn { loc, .. }
        | ReactiveTerminal::If { loc, .. }
        | ReactiveTerminal::Label { loc, .. }
        | ReactiveTerminal::Try { loc, .. } => loc,
    };
    match loc {
        SourceLocation::Source(range) => Some(range.start.line),
        SourceLocation::Generated => None,
    }
}

fn pop_last_simple_statement_line(output: &mut String) -> Option<String> {
    let mut end = output.len();
    while end > 0 && output.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let start = output[..end].rfind('\n').map_or(0, |idx| idx + 1);
    let line = &output[start..end];
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.contains(':')
        || !trimmed.ends_with(';')
        || trimmed.starts_with("$[")
        || trimmed.starts_with("break ")
        || trimmed.starts_with("continue ")
        || !is_literal_init_assignment_line(trimmed)
    {
        return None;
    }
    let trimmed_owned = trimmed.to_string();
    output.truncate(start);
    Some(trimmed_owned)
}

fn is_literal_init_assignment_line(line: &str) -> bool {
    let trimmed = line.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("const ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("let ") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("var ") {
        rest
    } else {
        trimmed
    };
    body.ends_with(" = [];")
        || body.ends_with(" = {};")
        || body.ends_with("=[];")
        || body.ends_with("={};")
}

fn next_emitted_statement_is_cache_store(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start_idx: usize,
) -> Option<bool> {
    preview_next_emitted_statement(cx, block, start_idx)
        .as_deref()
        .map(|stmt| stmt.trim_start().starts_with("$["))
}

fn preview_next_emitted_statement(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start_idx: usize,
) -> Option<String> {
    let debug = std::env::var("DEBUG_LABEL_ADJ").is_ok();
    let temp_snapshot = cx.snapshot_temps();
    let mut idx = start_idx;
    let mut first_stmt = None;
    while let Some(stmt) = block.get(idx) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        if let Some(code) = codegen_instruction_nullable(cx, instr) {
            first_stmt = Some(code);
            break;
        }
        idx += 1;
    }
    cx.restore_temps(temp_snapshot);
    if debug {
        eprintln!(
            "[LABEL_ADJ] next-preview idx={} stmt={:?}",
            start_idx, first_stmt
        );
    }
    first_stmt
}

fn should_insert_blank_after_labeled_if(
    term_stmt: &ReactiveTerminalStatement,
    block: &[ReactiveStatement],
    idx: usize,
) -> bool {
    let Some(label) = term_stmt.label.as_ref() else {
        return false;
    };
    if label.implicit {
        return false;
    }
    let ReactiveTerminal::If {
        consequent,
        alternate,
        loc,
        ..
    } = &term_stmt.terminal
    else {
        return false;
    };
    if alternate.is_some() {
        return false;
    }
    let SourceLocation::Source(range) = loc else {
        return false;
    };
    let Some((target, prefix)) = split_trailing_break(consequent) else {
        return false;
    };
    if target != label.id || !prefix.is_empty() {
        return false;
    }
    let term_end_line = range.end.line;
    let mut cursor = idx + 1;
    while let Some(stmt) = block.get(cursor) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            return false;
        };
        if let Some(next_line) = reactive_instruction_start_line(instr) {
            return next_line >= term_end_line + 2;
        }
        cursor += 1;
    }
    false
}

fn should_insert_blank_after_try_call_followup(
    _cx: &mut Context,
    term_stmt: &ReactiveTerminalStatement,
    block: &[ReactiveStatement],
    idx: usize,
) -> bool {
    let debug = std::env::var("DEBUG_TRY_SPACE").is_ok();
    let ReactiveTerminal::Try { handler, loc, .. } = &term_stmt.terminal else {
        if debug {
            eprintln!("[TRY_SPACE] skip: non-try");
        }
        return false;
    };
    let SourceLocation::Source(range) = loc else {
        if debug {
            eprintln!("[TRY_SPACE] skip: generated-loc");
        }
        return false;
    };
    let mut cursor = idx + 1;
    let mut next_line = None;
    while let Some(stmt) = block.get(cursor) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        if let Some(line) = reactive_instruction_start_line(instr) {
            next_line = Some(line);
            break;
        }
        cursor += 1;
    }
    let Some(next_line) = next_line else {
        if debug {
            eprintln!("[TRY_SPACE] skip: no-following-source-line");
        }
        return false;
    };
    if next_line != range.end.line + 1 {
        if debug {
            eprintln!(
                "[TRY_SPACE] skip: line-gap term_end={} next_line={}",
                range.end.line, next_line
            );
        }
        return false;
    }
    let mut cursor = idx + 1;
    let mut is_call = false;
    while let Some(stmt) = block.get(cursor) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        let Some(line) = reactive_instruction_start_line(instr) else {
            cursor += 1;
            continue;
        };
        if line != next_line {
            break;
        }
        if matches!(
            &instr.value,
            InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
        ) {
            is_call = true;
            break;
        }
        cursor += 1;
    }
    if debug {
        eprintln!(
            "[TRY_SPACE] decision: term={}..{} next_line={} call={} handler_len={}",
            range.start.line,
            range.end.line,
            next_line,
            is_call,
            handler.len()
        );
    }
    is_call
}

fn should_insert_blank_after_if_trailing_labeled_break(
    cx: &mut Context,
    term_stmt: &ReactiveTerminalStatement,
    block: &[ReactiveStatement],
    idx: usize,
) -> bool {
    let debug = std::env::var("DEBUG_IF_BREAK_SPACE").is_ok();
    let ReactiveTerminal::If {
        consequent,
        alternate,
        ..
    } = &term_stmt.terminal
    else {
        return false;
    };
    let breaks_in_consequent = branch_has_trailing_labeled_break(consequent);
    let breaks_in_alternate = alternate
        .as_ref()
        .is_some_and(|branch| branch_has_trailing_labeled_break(branch));
    if !breaks_in_consequent && !breaks_in_alternate {
        if debug {
            eprintln!("[IF_BREAK_SPACE] skip: no trailing labeled break");
        }
        return false;
    }
    if debug {
        eprintln!(
            "[IF_BREAK_SPACE] candidate: idx={} break_cons={} break_alt={}",
            idx, breaks_in_consequent, breaks_in_alternate
        );
    }
    let mut cursor = idx + 1;
    let mut saw_source_instruction = false;
    while let Some(stmt) = block.get(cursor) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            if saw_source_instruction
                && matches!(
                    stmt,
                    ReactiveStatement::Terminal(ReactiveTerminalStatement {
                        terminal: ReactiveTerminal::If { .. },
                        ..
                    })
                )
            {
                if debug {
                    eprintln!(
                        "[IF_BREAK_SPACE] decision: insert before following terminal-if at cursor={}",
                        cursor
                    );
                }
                return true;
            }
            if debug {
                let kind = match stmt {
                    ReactiveStatement::Terminal(term_stmt) => match &term_stmt.terminal {
                        ReactiveTerminal::If { .. } => "terminal-if",
                        ReactiveTerminal::Switch { .. } => "terminal-switch",
                        ReactiveTerminal::Try { .. } => "terminal-try",
                        ReactiveTerminal::Return { .. } => "terminal-return",
                        ReactiveTerminal::Break { .. } => "terminal-break",
                        ReactiveTerminal::Continue { .. } => "terminal-continue",
                        ReactiveTerminal::Throw { .. } => "terminal-throw",
                        ReactiveTerminal::DoWhile { .. } => "terminal-do-while",
                        ReactiveTerminal::For { .. } => "terminal-for",
                        ReactiveTerminal::ForOf { .. } => "terminal-for-of",
                        ReactiveTerminal::ForIn { .. } => "terminal-for-in",
                        ReactiveTerminal::While { .. } => "terminal-while",
                        ReactiveTerminal::Label { .. } => "terminal-label",
                    },
                    ReactiveStatement::Scope(_) => "scope",
                    ReactiveStatement::PrunedScope(_) => "pruned-scope",
                    ReactiveStatement::Instruction(_) => "instruction",
                };
                eprintln!(
                    "[IF_BREAK_SPACE] stop: non-instruction at cursor={} kind={}",
                    cursor, kind
                );
            }
            return false;
        };
        if reactive_instruction_start_line(instr).is_none() {
            if debug {
                eprintln!("[IF_BREAK_SPACE] skip: no source line at cursor={}", cursor);
            }
            cursor += 1;
            continue;
        }
        saw_source_instruction = true;
        let temp_snapshot = cx.snapshot_temps();
        let next_stmt = codegen_instruction_nullable(cx, instr);
        cx.restore_temps(temp_snapshot);
        let Some(next_stmt) = next_stmt else {
            if debug {
                eprintln!(
                    "[IF_BREAK_SPACE] skip: instruction emitted none at cursor={}",
                    cursor
                );
            }
            cursor += 1;
            continue;
        };
        let trimmed = next_stmt.trim_start();
        let should_insert = !trimmed.starts_with("let ")
            && !trimmed.starts_with("const ")
            && !trimmed.starts_with("var ");
        if debug {
            eprintln!(
                "[IF_BREAK_SPACE] decision: cursor={} stmt={:?} insert={}",
                cursor,
                trimmed.lines().next().unwrap_or(trimmed),
                should_insert
            );
        }
        return should_insert;
    }
    if debug {
        eprintln!("[IF_BREAK_SPACE] stop: end of block");
    }
    false
}

fn branch_has_trailing_labeled_break(branch: &[ReactiveStatement]) -> bool {
    matches!(
        branch.last(),
        Some(ReactiveStatement::Terminal(ReactiveTerminalStatement {
            terminal: ReactiveTerminal::Break {
                target_kind: ReactiveTerminalTargetKind::Labeled,
                ..
            },
            ..
        }))
    )
}

fn labeled_statement_needs_block_wrapper(stmt: &str) -> bool {
    let Some(first_stmt_end) = first_top_level_statement_end(stmt) else {
        return false;
    };
    has_non_trivia_after(stmt, first_stmt_end)
}

fn first_top_level_statement_end(code: &str) -> Option<usize> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ScanState {
        Normal,
        SingleQuote,
        DoubleQuote,
        TemplateQuote,
        LineComment,
        BlockComment,
    }

    let bytes = code.as_bytes();
    let mut i = 0usize;
    let mut paren = 0u32;
    let mut bracket = 0u32;
    let mut brace = 0u32;
    let mut state = ScanState::Normal;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            ScanState::LineComment => {
                if b == b'\n' {
                    state = ScanState::Normal;
                }
                i += 1;
                continue;
            }
            ScanState::BlockComment => {
                if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            ScanState::SingleQuote => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'\'' {
                    state = ScanState::Normal;
                }
                i += 1;
                continue;
            }
            ScanState::DoubleQuote => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    state = ScanState::Normal;
                }
                i += 1;
                continue;
            }
            ScanState::TemplateQuote => {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'`' {
                    state = ScanState::Normal;
                }
                i += 1;
                continue;
            }
            ScanState::Normal => {}
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            state = ScanState::LineComment;
            i += 2;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            state = ScanState::BlockComment;
            i += 2;
            continue;
        }
        if b == b'\'' {
            state = ScanState::SingleQuote;
            i += 1;
            continue;
        }
        if b == b'"' {
            state = ScanState::DoubleQuote;
            i += 1;
            continue;
        }
        if b == b'`' {
            state = ScanState::TemplateQuote;
            i += 1;
            continue;
        }

        match b {
            b'(' => paren += 1,
            b')' => paren = paren.saturating_sub(1),
            b'[' => bracket += 1,
            b']' => bracket = bracket.saturating_sub(1),
            b'{' => brace += 1,
            b'}' => {
                brace = brace.saturating_sub(1);
                if paren == 0 && bracket == 0 && brace == 0 {
                    let end = i + 1;
                    if let Some(next) = next_non_trivia_index(code, end)
                        && (slice_starts_with_keyword(code, next, "else")
                            || slice_starts_with_keyword(code, next, "catch")
                            || slice_starts_with_keyword(code, next, "finally")
                            || slice_starts_with_keyword(code, next, "while"))
                    {
                        i += 1;
                        continue;
                    }
                    return Some(end);
                }
            }
            b';' => {
                if paren == 0 && bracket == 0 && brace == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }

    None
}

fn has_non_trivia_after(code: &str, start: usize) -> bool {
    next_non_trivia_index(code, start).is_some()
}

fn next_non_trivia_index(code: &str, start: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut i = start;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < bytes.len() {
                i += 2;
            }
            continue;
        }
        return Some(i);
    }
    None
}

fn slice_starts_with_keyword(code: &str, start: usize, kw: &str) -> bool {
    let rest = &code[start..];
    if !rest.starts_with(kw) {
        return false;
    }
    let after = rest[kw.len()..].chars().next();
    after.is_none_or(|ch| !(ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()))
}

fn maybe_emit_reused_optional_dependency_reads(
    cx: &mut Context,
    output: &mut String,
    scope: &ReactiveScope,
    following_stmts: &[ReactiveStatement],
) {
    let mut current_deps: Vec<ReactiveScopeDependency> = scope
        .dependencies
        .iter()
        .map(|dep| truncate_ref_current_dep(dep, &cx.stable_ref_decls))
        .collect();
    sort_scope_dependencies_for_codegen(cx, &mut current_deps);

    let mut seen_current: HashSet<String> = HashSet::new();
    let mut optional_deps: Vec<ReactiveScopeDependency> = Vec::new();
    for dep in current_deps {
        if !dep.path.iter().any(|entry| entry.optional) {
            continue;
        }
        let key = format_dependency_name(&dep);
        if seen_current.insert(key) {
            optional_deps.push(dep);
        }
    }
    if optional_deps.is_empty() {
        return;
    }

    let optional_keys: HashSet<String> = optional_deps.iter().map(format_dependency_name).collect();
    let mut reused_by_later_scope = false;
    for stmt in following_stmts {
        let maybe_scope = match stmt {
            ReactiveStatement::Scope(scope_block) => Some(&scope_block.scope),
            ReactiveStatement::PrunedScope(scope_block) => Some(&scope_block.scope),
            _ => None,
        };
        let Some(next_scope) = maybe_scope else {
            continue;
        };
        if next_scope.dependencies.iter().any(|dep| {
            let dep = truncate_ref_current_dep(dep, &cx.stable_ref_decls);
            optional_keys.contains(&format_dependency_name(&dep))
        }) {
            reused_by_later_scope = true;
            break;
        }
    }
    if !reused_by_later_scope {
        return;
    }

    for dep in optional_deps {
        let dep_expr = codegen_dependency(cx, &dep);
        if dep_expr.contains("?.") && cx.emitted_optional_dep_reads.insert(dep_expr.clone()) {
            if let Some(stmt) = render_reactive_expression_statement_ast(&dep_expr) {
                output.push_str(&stmt);
            } else {
                output.push_str(&format!("{};\n", dep_expr));
            }
        }
    }
}

fn maybe_codegen_fused_nullish_self_reassign(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    let first_lvalue = first_instr.lvalue.as_ref()?;
    if first_lvalue.identifier.name.is_none() || !cx.has_declared(&first_lvalue.identifier) {
        return None;
    }
    if matches!(
        first_instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::ObjectMethod { .. }
    ) {
        return None;
    }

    let mut cursor = start + 1;
    let (second_lvalue, right_place) = loop {
        let ReactiveStatement::Instruction(instr) = block.get(cursor)? else {
            return None;
        };
        if let InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } = &instr.value
        {
            if *operator != LogicalOperator::NullishCoalescing {
                return None;
            }
            let second_lvalue = instr.lvalue.as_ref()?;
            if left.identifier.declaration_id != first_lvalue.identifier.declaration_id {
                return None;
            }
            break (second_lvalue, right);
        }

        if !is_fusable_inline_temp_instruction(instr)
            || !materialize_fusable_temp_instruction(cx, instr)
        {
            return None;
        }
        cursor += 1;
    };

    let lhs = codegen_instruction_value_ev(cx, &first_instr.value)
        .wrap_if_needed(ExprPrecedence::NullishCoalescing);
    let rhs = codegen_place_with_min_prec(cx, right_place, ExprPrecedence::NullishCoalescing);
    let fused_expr = format!("{} ?? {}", lhs, rhs);

    if first_lvalue.identifier.name.is_none() {
        let ev = ExprValue::new(fused_expr, ExprPrecedence::NullishCoalescing);
        cx.temp
            .insert(first_lvalue.identifier.declaration_id, Some(ev.clone()));
        if second_lvalue.identifier.declaration_id != first_lvalue.identifier.declaration_id {
            cx.temp
                .insert(second_lvalue.identifier.declaration_id, Some(ev));
        }
    } else {
        let name = identifier_name_with_cx(cx, &first_lvalue.identifier);
        if cx.has_declared(&first_lvalue.identifier) {
            output.push_str(&render_reactive_assignment_statement_ast(
                &name,
                &fused_expr,
            )?);
        } else {
            cx.declare(&first_lvalue.identifier);
            output.push_str(&render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Const,
                &name,
                Some(&fused_expr),
            )?);
        }
        if second_lvalue.identifier.id != first_lvalue.identifier.id {
            cx.set_temp_expr(&second_lvalue.identifier, Some(ExprValue::primary(name)));
        }
    }

    Some(cursor - start + 1)
}

fn split_top_level_statement_chunks_global(code: &str) -> Option<Vec<String>> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape = false;
    let mut line_comment = false;
    let mut block_comment = false;
    let mut template_stack: Vec<usize> = Vec::new();
    let mut chars = code.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if line_comment {
            if ch == '\n' {
                line_comment = false;
            }
            continue;
        }

        if block_comment {
            if ch == '*' && chars.peek().is_some_and(|(_, next)| *next == '/') {
                chars.next();
                block_comment = false;
            }
            continue;
        }

        if in_single_quote {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '\'' {
                in_single_quote = false;
            }
            continue;
        }

        if in_double_quote {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_double_quote = false;
            }
            continue;
        }

        if let Some(template_expr_depth) = template_stack.last_mut() {
            if *template_expr_depth == 0 {
                if escape {
                    escape = false;
                    continue;
                }
                match ch {
                    '\\' => escape = true,
                    '`' => {
                        template_stack.pop();
                    }
                    '$' if chars.peek().is_some_and(|(_, next)| *next == '{') => {
                        chars.next();
                        *template_expr_depth = 1;
                    }
                    _ => {}
                }
                continue;
            }

            match ch {
                '/' if chars.peek().is_some_and(|(_, next)| *next == '/') => {
                    chars.next();
                    line_comment = true;
                }
                '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                    chars.next();
                    block_comment = true;
                }
                '\'' => in_single_quote = true,
                '"' => in_double_quote = true,
                '`' => template_stack.push(0),
                '{' => *template_expr_depth += 1,
                '}' => {
                    *template_expr_depth = template_expr_depth.saturating_sub(1);
                }
                _ => {}
            }
            continue;
        }

        match ch {
            '/' if chars.peek().is_some_and(|(_, next)| *next == '/') => {
                chars.next();
                line_comment = true;
            }
            '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                chars.next();
                block_comment = true;
            }
            '\'' => in_single_quote = true,
            '"' => in_double_quote = true,
            '`' => template_stack.push(0),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ';' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                let chunk = code[start..=idx].trim();
                if !chunk.is_empty() {
                    chunks.push(chunk.to_string());
                }
                start = idx + 1;
            }
            _ => {}
        }
    }

    if in_single_quote
        || in_double_quote
        || line_comment
        || block_comment
        || !template_stack.is_empty()
        || paren_depth != 0
        || bracket_depth != 0
        || brace_depth != 0
    {
        return None;
    }

    if !code[start..].trim().is_empty() {
        return None;
    }

    if chunks.is_empty() {
        None
    } else {
        Some(chunks)
    }
}

fn extract_simple_expression_statement_global(stmt: &str) -> Option<String> {
    let chunks = split_top_level_statement_chunks_global(stmt)?;
    let [chunk] = chunks.as_slice() else {
        return None;
    };
    let expr = chunk.trim_end_matches(';').trim();
    if expr.is_empty() {
        return None;
    }
    if expr.starts_with("let ")
        || expr.starts_with("const ")
        || expr.starts_with("if ")
        || expr.starts_with("while ")
        || expr.starts_with("for ")
        || expr.starts_with("do ")
        || expr.starts_with("switch ")
        || expr.starts_with("return ")
        || expr.starts_with("throw ")
        || expr.starts_with("try ")
        || expr.starts_with("break ")
        || expr.starts_with("continue ")
    {
        return None;
    }
    Some(expr.to_string())
}

fn extract_initializer_rhs_global(stmt: &str) -> Option<String> {
    let chunks = split_top_level_statement_chunks_global(stmt)?;
    let [chunk] = chunks.as_slice() else {
        return None;
    };
    let without_semi = chunk.trim_end_matches(';').trim();
    for prefix in ["const ", "let ", "var "] {
        let Some(rest) = without_semi.strip_prefix(prefix) else {
            continue;
        };
        let (_, rhs) = rest.split_once('=')?;
        let rhs = rhs.trim();
        if rhs.is_empty() {
            return None;
        }
        return Some(rhs.to_string());
    }
    None
}

fn normalize_fusion_match_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut prev_was_whitespace = false;

    for ch in text.chars() {
        if ch.is_whitespace() {
            if !prev_was_whitespace {
                normalized.push(' ');
                prev_was_whitespace = true;
            }
        } else {
            normalized.push(ch);
            prev_was_whitespace = false;
        }
    }

    normalized.trim().to_string()
}

fn contains_all_bridge_exprs_normalized(text: &str, bridge_exprs: &[String]) -> bool {
    let normalized_text = normalize_fusion_match_text(text);
    bridge_exprs.iter().all(|bridge_expr| {
        let normalized_expr = normalize_fusion_match_text(bridge_expr);
        normalized_text.contains(&normalized_expr)
    })
}

fn is_assignment_like_sequence_expr_global(expr: &str) -> bool {
    let trimmed = expr.trim();
    if trimmed.starts_with("let ") || trimmed.starts_with("const ") {
        return false;
    }
    trimmed.contains(" = ")
}

fn wrap_sequence_expr_item_global(expr: &str) -> String {
    let trimmed = expr.trim();
    if is_assignment_like_sequence_expr_global(trimmed)
        && !(trimmed.starts_with('(') && trimmed.ends_with(')'))
    {
        format!("({trimmed})")
    } else {
        trimmed.to_string()
    }
}

fn parse_named_const_assignment_line(line: &str) -> Option<(String, String, String)> {
    let indent_len = line.len().saturating_sub(line.trim_start().len());
    let indent = line[..indent_len].to_string();
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("const ")?;
    let rest = rest.strip_suffix(';')?;
    let (lhs, rhs) = rest.split_once('=')?;
    let lhs = lhs.trim();
    if !is_simple_identifier_name(lhs) {
        return None;
    }
    let rhs = rhs.trim();
    if rhs.is_empty() {
        return None;
    }
    Some((indent, lhs.to_string(), rhs.to_string()))
}

fn parse_named_temp_binding_line(line: &str) -> Option<(String, String, String)> {
    let indent_len = line.len().saturating_sub(line.trim_start().len());
    let indent = line[..indent_len].to_string();
    let trimmed = line.trim();
    let rest = trimmed
        .strip_prefix("const ")
        .or_else(|| trimmed.strip_prefix("let "))?;
    let rest = rest.strip_suffix(';')?;
    let (lhs, rhs) = rest.split_once('=')?;
    let lhs = lhs.trim();
    let rhs = rhs.trim();
    if !is_simple_identifier_name(lhs) || rhs.is_empty() {
        return None;
    }
    Some((indent, lhs.to_string(), rhs.to_string()))
}

fn parse_assignment_statement_line(line: &str) -> Option<(String, String, String)> {
    let indent_len = line.len().saturating_sub(line.trim_start().len());
    let indent = line[..indent_len].to_string();
    let trimmed = line.trim();
    let body = trimmed.strip_suffix(';')?;
    let (lhs, rhs) = body.split_once('=')?;
    let lhs = lhs.trim();
    let rhs = rhs.trim();
    if lhs.is_empty() || rhs.is_empty() {
        return None;
    }
    Some((indent, lhs.to_string(), rhs.to_string()))
}

fn parse_ternary_statement_line(line: &str) -> Option<(String, String, String, String)> {
    let indent_len = line.len().saturating_sub(line.trim_start().len());
    let indent = line[..indent_len].to_string();
    let trimmed = line.trim();
    let body = trimmed.strip_suffix(';')?;
    let (test, rest) = body.split_once(" ? ")?;
    let (consequent, alternate) = rest.rsplit_once(" : ")?;
    let test = test.trim();
    let consequent = consequent.trim();
    let alternate = alternate.trim();
    if test.is_empty() || consequent.is_empty() || alternate.is_empty() {
        return None;
    }
    Some((
        indent,
        test.to_string(),
        consequent.to_string(),
        alternate.to_string(),
    ))
}

fn rewrite_named_test_reassign_ternary_in_scope_computation(computation: &str) -> String {
    let had_trailing_newline = computation.ends_with('\n');
    let lines: Vec<String> = computation
        .trim_end_matches('\n')
        .split('\n')
        .map(|line| line.to_string())
        .collect();
    if lines.is_empty() {
        return computation.to_string();
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        if i + 3 < lines.len()
            && let Some((indent, test_name, test_expr)) =
                parse_named_const_assignment_line(&lines[i])
            && is_codegen_temp_name(&test_name)
            && let Some((_, temp_name, consequent_expr)) =
                parse_assignment_statement_line(&lines[i + 1])
            && is_codegen_temp_name(&temp_name)
            && let Some((_, assign_target, assign_rhs)) =
                parse_assignment_statement_line(&lines[i + 2])
            && let Some((_, ternary_test, ternary_consequent, ternary_alternate)) =
                parse_ternary_statement_line(&lines[i + 3])
            && ternary_test == test_name
            && ternary_consequent == temp_name
            && contains_identifier_token(&ternary_alternate, &assign_target)
            && assign_rhs == "[]"
            && test_expr.starts_with("props.")
            && consequent_expr.contains(&format!("{} = []", assign_target))
            && consequent_expr.contains(&format!("{}.push(", assign_target))
            && ternary_alternate.contains(&format!("{}.push(", assign_target))
            && i > 0
            && lines[i - 1]
                .trim()
                .starts_with(&format!("{}.push(", assign_target))
        {
            let assign_expr = format!("{} = {}", assign_target, assign_rhs);
            out.push(format!(
                "{}{} ? {} : (({}), {});",
                indent, test_expr, consequent_expr, assign_expr, ternary_alternate
            ));
            i += 4;
            continue;
        }
        out.push(lines[i].clone());
        i += 1;
    }

    let mut rewritten = out.join("\n");
    if had_trailing_newline {
        rewritten.push('\n');
    }
    rewritten
}

fn rewrite_named_temp_ternary_in_scope_computation(computation: &str) -> String {
    let had_trailing_newline = computation.ends_with('\n');
    let lines: Vec<String> = computation
        .trim_end_matches('\n')
        .split('\n')
        .map(|line| line.to_string())
        .collect();
    if lines.is_empty() {
        return computation.to_string();
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0usize;
    while i < lines.len() {
        let Some((indent, first_name, first_expr)) = parse_named_temp_binding_line(&lines[i])
        else {
            out.push(lines[i].clone());
            i += 1;
            continue;
        };
        if !is_codegen_temp_name(&first_name) {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }

        let mut bindings: HashMap<String, String> = HashMap::new();
        bindings.insert(first_name, first_expr);
        let mut j = i + 1;
        while j < lines.len() {
            let Some((next_indent, name, expr)) = parse_named_temp_binding_line(&lines[j]) else {
                break;
            };
            if next_indent != indent || !is_codegen_temp_name(&name) {
                break;
            }
            bindings.insert(name, expr);
            j += 1;
        }

        let Some((ternary_indent, test, consequent, alternate)) = lines
            .get(j)
            .and_then(|line| parse_ternary_statement_line(line))
        else {
            out.push(lines[i].clone());
            i += 1;
            continue;
        };
        if ternary_indent != indent {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }

        let rewritten_test = bindings.get(&test).cloned().unwrap_or_else(|| test.clone());
        let rewritten_consequent = bindings
            .get(&consequent)
            .cloned()
            .unwrap_or_else(|| consequent.clone());
        let rewritten_alternate = bindings
            .get(&alternate)
            .cloned()
            .unwrap_or_else(|| alternate.clone());
        let changed = rewritten_test != test
            || rewritten_consequent != consequent
            || rewritten_alternate != alternate;
        if !changed {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }

        out.push(format!(
            "{}{} ? {} : {};",
            indent, rewritten_test, rewritten_consequent, rewritten_alternate
        ));
        i = j + 1;
    }

    let mut rewritten = out.join("\n");
    if had_trailing_newline {
        rewritten.push('\n');
    }
    rewritten
}

struct DualReassignScopeBranch {
    decl_id: DeclarationId,
    dep_expr: String,
    branch_expr: String,
    reassign_ident: Identifier,
    reassign_name: String,
}

fn parse_dual_reassign_scope_branch(
    cx: &Context,
    scope_block: &ReactiveScopeBlock,
) -> Option<DualReassignScopeBranch> {
    macro_rules! skip {
        ($($arg:tt)*) => {{
            debug_codegen_expr(
                "fused-dual-reassign-branch-parse-skip",
                format!("scope={} {}", scope_block.scope.id.0, format!($($arg)*)),
            );
            return None;
        }};
    }
    if scope_block.scope.dependencies.len() != 1
        || scope_block.scope.reassignments.len() != 1
        || scope_block.scope.declarations.len() != 1
    {
        skip!(
            "shape deps={} reassignments={} decls={}",
            scope_block.scope.dependencies.len(),
            scope_block.scope.reassignments.len(),
            scope_block.scope.declarations.len()
        );
    }

    let decl_ident = &scope_block.scope.declarations.values().next()?.identifier;
    let decl_id = decl_ident.declaration_id;
    let raw_dep = scope_block.scope.dependencies.first()?;
    let dep_for_codegen = truncate_ref_current_dep(raw_dep, &cx.stable_ref_decls);

    let mut probe = cx.clone();
    let dep_expr = codegen_dependency(&mut probe, &dep_for_codegen);
    let reassign_ident = scope_block.scope.reassignments.first()?.clone();
    let reassign_name = identifier_name_with_cx(&mut probe, &reassign_ident);
    if reassign_name.is_empty() {
        skip!("empty-reassign-name");
    }

    let computation = codegen_scope_computation_no_reset(
        &mut probe,
        &scope_block.scope,
        &scope_block.instructions,
    );
    let decl_name = identifier_name_with_cx(&mut probe, decl_ident);
    let mut pre_exprs: Vec<String> = Vec::new();
    let mut decl_rhs: Option<String> = None;
    for line in computation.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rhs) = trimmed
            .strip_prefix(&format!("const {decl_name} = "))
            .or_else(|| trimmed.strip_prefix(&format!("let {decl_name} = ")))
            .and_then(|rhs| rhs.strip_suffix(';'))
            .map(|rhs| rhs.trim().to_string())
        {
            decl_rhs = Some(rhs);
            continue;
        }
        let Some(expr) = extract_simple_expression_statement_global(trimmed) else {
            skip!("non-expression-line `{}`", trimmed);
        };
        if let Some(rhs) = expr
            .strip_prefix(&format!("{decl_name} = "))
            .map(|rhs| rhs.trim().to_string())
        {
            decl_rhs = Some(rhs);
            continue;
        }
        pre_exprs.push(expr);
    }

    let Some(decl_rhs) = decl_rhs else {
        skip!(
            "missing-decl-assign decl={} computation={:?}",
            decl_name,
            computation
        );
    };
    if pre_exprs
        .iter()
        .any(|expr| !expr.trim_start().starts_with(&format!("{reassign_name} =")))
    {
        skip!(
            "unexpected-prefix pre_exprs={:?} target={} computation={:?}",
            pre_exprs,
            reassign_name,
            computation
        );
    }

    let mut seq_items: Vec<String> = pre_exprs
        .iter()
        .map(|expr| wrap_sequence_expr_item_global(expr))
        .collect();
    seq_items.push(decl_rhs);
    let branch_expr = if seq_items.len() == 1 {
        seq_items[0].clone()
    } else {
        format!("({})", seq_items.join(", "))
    };
    if !contains_identifier_token(&branch_expr, &reassign_name) {
        skip!(
            "branch-missing-target branch={} target={}",
            branch_expr,
            reassign_name
        );
    }

    Some(DualReassignScopeBranch {
        decl_id,
        dep_expr,
        branch_expr,
        reassign_ident,
        reassign_name,
    })
}

fn maybe_codegen_fused_named_test_dual_reassign_scope_ternary_return(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    macro_rules! skip {
        ($($arg:tt)*) => {{
            debug_codegen_expr(
                "fused-named-test-dual-reassign-scope-ternary-return-skip",
                format!("start={} {}", start, format!($($arg)*)),
            );
            return None;
        }};
    }

    let ReactiveStatement::Instruction(test_decl_instr) = block.get(start)? else {
        return None;
    };
    let test_decl_lvalue = test_decl_instr.lvalue.as_ref()?;
    let test_name = identifier_name_static(&test_decl_lvalue.identifier);
    if !is_codegen_temp_name(&test_name) || cx.has_declared(&test_decl_lvalue.identifier) {
        skip!(
            "bad-test-decl name={} declared={}",
            test_name,
            cx.has_declared(&test_decl_lvalue.identifier)
        );
    }

    let ReactiveStatement::Scope(first_scope) = block.get(start + 1)? else {
        skip!("next-not-scope");
    };
    let ReactiveStatement::Scope(second_scope) = block.get(start + 2)? else {
        skip!("next2-not-scope");
    };
    let ReactiveStatement::Instruction(ternary_instr) = block.get(start + 3)? else {
        skip!("next3-not-instruction");
    };
    if ternary_instr.lvalue.is_some() {
        skip!("ternary-has-lvalue");
    }
    let InstructionValue::Ternary {
        test,
        consequent,
        alternate,
        ..
    } = &ternary_instr.value
    else {
        skip!("next3-not-ternary");
    };
    if test.identifier.declaration_id != test_decl_lvalue.identifier.declaration_id {
        skip!(
            "test-decl-mismatch ternary={} test_decl={}",
            test.identifier.declaration_id.0,
            test_decl_lvalue.identifier.declaration_id.0
        );
    }

    let Some(first) = parse_dual_reassign_scope_branch(cx, first_scope) else {
        skip!("first-scope-parse-failed");
    };
    let Some(second) = parse_dual_reassign_scope_branch(cx, second_scope) else {
        skip!("second-scope-parse-failed");
    };
    if first.reassign_ident.declaration_id != second.reassign_ident.declaration_id
        || first.reassign_name != second.reassign_name
    {
        skip!(
            "reassign-target-mismatch first={} second={}",
            first.reassign_name,
            second.reassign_name
        );
    }

    let (consequent_branch, alternate_branch) = if consequent.identifier.declaration_id
        == first.decl_id
        && alternate.identifier.declaration_id == second.decl_id
    {
        (&first, &second)
    } else if consequent.identifier.declaration_id == second.decl_id
        && alternate.identifier.declaration_id == first.decl_id
    {
        (&second, &first)
    } else {
        skip!(
            "branch-decl-mismatch cons={} alt={} first={} second={}",
            consequent.identifier.declaration_id.0,
            alternate.identifier.declaration_id.0,
            first.decl_id.0,
            second.decl_id.0
        );
    };

    let mut cursor = start + 4;
    let mut return_decl = first.reassign_ident.declaration_id;
    let mut bridged_return_decls: Vec<DeclarationId> = Vec::new();
    loop {
        let Some(stmt) = block.get(cursor) else {
            skip!("missing-return-tail");
        };
        match stmt {
            ReactiveStatement::Instruction(instr) if is_ignorable_bridge_interstitial(instr) => {
                if let Some(lvalue) = &instr.lvalue {
                    match &instr.value {
                        InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. }
                            if place.identifier.declaration_id == return_decl =>
                        {
                            return_decl = lvalue.identifier.declaration_id;
                            bridged_return_decls.push(return_decl);
                        }
                        _ => {}
                    }
                }
                cursor += 1;
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let ReactiveTerminal::Return { value, .. } = &term_stmt.terminal else {
                    skip!("tail-terminal-not-return");
                };
                if value.identifier.declaration_id != return_decl || cursor + 1 != block.len() {
                    skip!(
                        "return-check-failed value_decl={} expected_decl={} tail_remaining={}",
                        value.identifier.declaration_id.0,
                        return_decl.0,
                        block.len().saturating_sub(cursor + 1)
                    );
                }
                break;
            }
            _ => skip!("tail-non-ignorable-stmt"),
        }
    }

    let test_ev = codegen_instruction_value_ev(cx, &test_decl_instr.value);
    let cond_expr = if test_ev.prec <= ExprPrecedence::Assignment {
        format!("({})", test_ev.expr)
    } else {
        test_ev.expr
    };

    let cache_var = cx.synthesize_name("$");
    let alternate_dep_slot = cx.alloc_cache_slot();
    let cond_slot = cx.alloc_cache_slot();
    let consequent_dep_slot = cx.alloc_cache_slot();
    let output_slot = cx.alloc_cache_slot();

    let target_name = first.reassign_name.clone();
    if !cx.has_declared(&first.reassign_ident) {
        output.push_str(&render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            &target_name,
            None,
        )?);
        cx.mark_decl_runtime_emitted(first.reassign_ident.declaration_id);
    }
    cx.declare(&first.reassign_ident);

    let guard_expr = format!(
        "{}[{}] !== {} || {}[{}] !== {} || {}[{}] !== {}",
        cache_var,
        alternate_dep_slot,
        alternate_branch.dep_expr,
        cache_var,
        cond_slot,
        cond_expr,
        cache_var,
        consequent_dep_slot,
        consequent_branch.dep_expr,
    );
    let consequent = format!(
        "{}{}{}{}",
        render_reactive_expression_statement_ast(&format!(
            "{} ? {} : {}",
            cond_expr, consequent_branch.branch_expr, alternate_branch.branch_expr
        ))?,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, alternate_dep_slot, alternate_branch.dep_expr
        ))?,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, cond_slot, cond_expr
        ))?,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, consequent_dep_slot, consequent_branch.dep_expr
        ))?,
    );
    let consequent = format!(
        "{}{}",
        consequent,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, output_slot, target_name
        ))?,
    );
    let alternate = render_reactive_assignment_statement_ast(
        &target_name,
        &format!("{}[{}]", cache_var, output_slot),
    )?;
    output.push_str(&render_reactive_if_statement_ast(
        &guard_expr,
        &consequent,
        Some(&alternate),
    )?);
    for decl_id in bridged_return_decls {
        cx.temp
            .insert(decl_id, Some(ExprValue::primary(target_name.clone())));
    }
    debug_codegen_expr(
        "fused-named-test-dual-reassign-scope-ternary-return",
        format!(
            "target={} cond={} deps=[{},{}]",
            target_name, cond_expr, alternate_branch.dep_expr, consequent_branch.dep_expr
        ),
    );
    Some(cursor - start)
}

fn maybe_codegen_fused_named_test_scope_decl_ternary_statement(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(test_instr) = block.get(start)? else {
        return None;
    };
    let Some(test_lvalue) = &test_instr.lvalue else {
        return None;
    };
    test_lvalue.identifier.name.as_ref()?;
    let test_name = identifier_name_static(&test_lvalue.identifier);
    if !is_codegen_temp_name(&test_name) || cx.has_declared(&test_lvalue.identifier) {
        return None;
    }

    let ReactiveStatement::Scope(scope_block) = block.get(start + 1)? else {
        return None;
    };
    if scope_block.scope.dependencies.len() != 1
        || scope_block.scope.declarations.len() != 1
        || scope_block.scope.reassignments.len() != 1
    {
        return None;
    }
    let scope_decl_id = scope_block
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .declaration_id;

    let ReactiveStatement::Instruction(null_instr) = block.get(start + 2)? else {
        return None;
    };
    let Some(null_lvalue) = &null_instr.lvalue else {
        return None;
    };
    let InstructionValue::Primitive {
        value: PrimitiveValue::Null,
        ..
    } = &null_instr.value
    else {
        return None;
    };

    let ReactiveStatement::Instruction(ternary_instr) = block.get(start + 3)? else {
        return None;
    };
    if ternary_instr.lvalue.is_some() {
        return None;
    }
    let InstructionValue::Ternary {
        test,
        consequent,
        alternate,
        ..
    } = &ternary_instr.value
    else {
        return None;
    };
    if test.identifier.declaration_id != test_lvalue.identifier.declaration_id
        || consequent.identifier.declaration_id != scope_decl_id
        || alternate.identifier.declaration_id != null_lvalue.identifier.declaration_id
    {
        return None;
    }

    // Materialize default statements first so we can safely fall back.
    let test_stmt = codegen_instruction_nullable(cx, test_instr)?;
    let mut scope_stmt = String::new();
    codegen_reactive_scope(
        cx,
        &mut scope_stmt,
        &scope_block.scope,
        &scope_block.instructions,
    );
    let null_stmt = codegen_instruction_nullable(cx, null_instr);
    let ternary_stmt = match codegen_instruction_nullable(cx, ternary_instr) {
        Some(stmt) => stmt,
        None => {
            output.push_str(&test_stmt);
            output.push_str(&scope_stmt);
            if let Some(stmt) = null_stmt {
                output.push_str(&stmt);
            }
            return Some(4);
        }
    };

    let fallback_emit = |output: &mut String| {
        output.push_str(&test_stmt);
        output.push_str(&scope_stmt);
        if let Some(stmt) = &null_stmt {
            output.push_str(stmt);
        }
        output.push_str(&ternary_stmt);
    };

    let test_ev = codegen_instruction_value_ev(cx, &test_instr.value);
    let cond_expr = if test_ev.prec <= ExprPrecedence::Assignment {
        format!("({})", test_ev.expr)
    } else {
        test_ev.expr
    };

    let mut lines: Vec<&str> = scope_stmt.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    if lines.len() < 5 {
        fallback_emit(output);
        return Some(4);
    }

    let decl_line = lines[0].trim();
    let Some(scope_decl_name) = decl_line
        .strip_prefix("let ")
        .and_then(|rest| rest.strip_suffix(';'))
    else {
        fallback_emit(output);
        return Some(4);
    };
    let if_line = lines[1].trim();
    let Some(if_cond_raw) = if_line
        .strip_prefix("if (")
        .and_then(|rest| rest.strip_suffix(") {"))
    else {
        fallback_emit(output);
        return Some(4);
    };
    if if_cond_raw.contains("||") {
        fallback_emit(output);
        return Some(4);
    }
    let Some((guard_lhs_raw, dep_expr_raw)) = if_cond_raw.split_once("!==") else {
        fallback_emit(output);
        return Some(4);
    };
    let guard_lhs = guard_lhs_raw.trim();
    let dep_expr = dep_expr_raw.trim().to_string();
    let Some(lb) = guard_lhs.find('[') else {
        fallback_emit(output);
        return Some(4);
    };
    let Some(rb) = guard_lhs.rfind(']') else {
        fallback_emit(output);
        return Some(4);
    };
    if rb <= lb {
        fallback_emit(output);
        return Some(4);
    }
    let cache_var = guard_lhs[..lb].trim().to_string();
    let dep_slot = guard_lhs[lb + 1..rb].trim().to_string();
    if cache_var.is_empty() || dep_slot.is_empty() {
        fallback_emit(output);
        return Some(4);
    }

    let Some(else_idx) = lines.iter().position(|line| line.trim() == "} else {") else {
        fallback_emit(output);
        return Some(4);
    };
    let Some(end_idx) = lines.iter().rposition(|line| line.trim() == "}") else {
        fallback_emit(output);
        return Some(4);
    };
    if else_idx <= 1 || end_idx <= else_idx {
        fallback_emit(output);
        return Some(4);
    }
    let body_lines = &lines[2..else_idx];
    let else_lines = &lines[else_idx + 1..end_idx];

    let mut pre_exprs: Vec<String> = Vec::new();
    let mut decl_rhs: Option<String> = None;
    let mut decl_slot: Option<String> = None;
    let mut cache_tail_lines: Vec<String> = Vec::new();
    for line in body_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == format!("{cache_var}[{dep_slot}] = {dep_expr};") {
            continue;
        }
        if let Some(after_cache) = trimmed.strip_prefix(&format!("{cache_var}["))
            && let Some((slot, rhs_part)) = after_cache.split_once(']')
            && rhs_part.trim() == format!("= {scope_decl_name};")
        {
            decl_slot = Some(slot.trim().to_string());
            continue;
        }
        if let Some(rhs) = trimmed
            .strip_prefix(&format!("{scope_decl_name} = "))
            .and_then(|rest| rest.strip_suffix(';'))
        {
            decl_rhs = Some(rhs.trim().to_string());
            continue;
        }
        if trimmed.starts_with(&format!("{cache_var}[")) {
            cache_tail_lines.push(trimmed.to_string());
            continue;
        }
        let Some(expr) = extract_simple_expression_statement_global(trimmed) else {
            fallback_emit(output);
            return Some(4);
        };
        pre_exprs.push(expr);
    }

    let Some(decl_rhs) = decl_rhs else {
        fallback_emit(output);
        return Some(4);
    };
    let Some(decl_slot) = decl_slot else {
        fallback_emit(output);
        return Some(4);
    };

    let mut else_passthrough: Vec<String> = Vec::new();
    for line in else_lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == format!("{scope_decl_name} = {cache_var}[{decl_slot}];") {
            continue;
        }
        else_passthrough.push(trimmed.to_string());
    }

    let mut seq_items: Vec<String> = pre_exprs
        .iter()
        .map(|expr| wrap_sequence_expr_item_global(expr))
        .collect();
    seq_items.push(decl_rhs);
    let consequent_expr = if seq_items.len() == 1 {
        seq_items[0].clone()
    } else {
        format!("({})", seq_items.join(", "))
    };

    let mut consequent = String::new();
    consequent.push_str(&render_reactive_expression_statement_ast(&format!(
        "{cond_expr} ? {consequent_expr} : null"
    ))?);
    consequent.push_str(&render_reactive_expression_statement_ast(&format!(
        "{}[{}] = {}",
        cache_var, dep_slot, cond_expr
    ))?);
    consequent.push_str(&render_reactive_expression_statement_ast(&format!(
        "{}[{}] = {}",
        cache_var, decl_slot, dep_expr
    ))?);
    for line in cache_tail_lines {
        consequent.push_str(&render_reactive_expression_statement_ast(&line)?);
    }
    let mut alternate = String::new();
    for line in else_passthrough {
        alternate.push_str(&render_reactive_expression_statement_ast(&line)?);
    }
    output.push_str(&render_reactive_if_statement_ast(
        &format!(
            "{}[{}] !== {} || {}[{}] !== {}",
            cache_var, dep_slot, cond_expr, cache_var, decl_slot, dep_expr
        ),
        &consequent,
        Some(&alternate),
    )?);
    debug_codegen_expr(
        "fused-named-test-scope-decl-ternary-statement",
        format!(
            "test={} dep_slot={} decl_slot={} dep_expr={}",
            cond_expr, dep_slot, decl_slot, dep_expr
        ),
    );
    Some(4)
}

fn maybe_codegen_fused_named_test_reassign_then_ternary_branch(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    macro_rules! skip {
        ($($arg:tt)*) => {{
            debug_codegen_expr(
                "fused-named-test-reassign-ternary-branch-skip",
                format!("start={} {}", start, format!($($arg)*)),
            );
            return None;
        }};
    }

    let ReactiveStatement::Instruction(test_decl_instr) = block.get(start)? else {
        return None;
    };
    let test_decl_lvalue = test_decl_instr.lvalue.as_ref()?;
    let temp_name = identifier_name_static(&test_decl_lvalue.identifier);
    if !is_codegen_temp_name(&temp_name) {
        skip!("first-not-temp name={}", temp_name);
    }
    let mut probe = cx.clone();
    let mut assign_idx: Option<usize> = None;
    let mut ternary_idx: Option<usize> = None;
    let mut assign_target_idents: Vec<Identifier> = Vec::new();
    let mut ternary_test: Option<Place> = None;
    let mut ternary_consequent: Option<Place> = None;
    let mut ternary_alternate: Option<Place> = None;
    let mut named_bridge_expr_by_decl: HashMap<DeclarationId, String> = HashMap::new();
    let same_assign_targets = |lhs: &[Identifier], rhs: &[Identifier]| -> bool {
        if lhs.len() != rhs.len() {
            return false;
        }
        let mut lhs_ids: Vec<u32> = lhs.iter().map(|ident| ident.declaration_id.0).collect();
        let mut rhs_ids: Vec<u32> = rhs.iter().map(|ident| ident.declaration_id.0).collect();
        lhs_ids.sort_unstable();
        rhs_ids.sort_unstable();
        lhs_ids == rhs_ids
    };

    let mut idx = start + 1;
    while idx < block.len() {
        let ReactiveStatement::Instruction(instr) = &block[idx] else {
            skip!("non-instruction-before-pattern idx={}", idx);
        };
        let is_temp_bridge = instr
            .lvalue
            .as_ref()
            .is_some_and(|lvalue| lvalue.identifier.name.is_none());
        if let Some(lvalue) = &instr.lvalue
            && let Some(name) = lvalue.identifier.name.as_ref().map(IdentifierName::value)
            && is_codegen_temp_name(name)
        {
            let bridge_ev = codegen_instruction_value_ev(&mut probe, &instr.value);
            named_bridge_expr_by_decl.insert(
                lvalue.identifier.declaration_id,
                bridge_ev.wrap_if_needed(ExprPrecedence::Conditional),
            );
            idx += 1;
            continue;
        }

        if instr.lvalue.is_none() {
            let mut reassign_targets: Option<Vec<Identifier>> = None;
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. }
                    if lvalue.kind == InstructionKind::Reassign =>
                {
                    if lvalue.place.identifier.name.is_none() {
                        skip!("candidate-assign-unnamed-target idx={}", idx);
                    }
                    reassign_targets = Some(vec![lvalue.place.identifier.clone()]);
                    let _ = value;
                }
                InstructionValue::Destructure { lvalue, .. }
                    if lvalue.kind == InstructionKind::Reassign =>
                {
                    let mut targets = Vec::new();
                    for operand in pattern_operands(&lvalue.pattern) {
                        if operand.identifier.name.is_some() {
                            targets.push(operand.identifier.clone());
                        }
                    }
                    if targets.is_empty() {
                        skip!("candidate-assign-destructure-no-named-target idx={}", idx);
                    }
                    reassign_targets = Some(targets);
                }
                _ => {}
            }

            if let Some(targets) = reassign_targets {
                if assign_target_idents.is_empty() {
                    assign_target_idents = targets;
                } else if !same_assign_targets(&assign_target_idents, &targets) {
                    skip!(
                        "candidate-assign-target-mismatch idx={} prev={:?} next={:?}",
                        idx,
                        assign_target_idents
                            .iter()
                            .map(|ident| ident.declaration_id.0)
                            .collect::<Vec<_>>(),
                        targets
                            .iter()
                            .map(|ident| ident.declaration_id.0)
                            .collect::<Vec<_>>()
                    );
                } else {
                    assign_target_idents = targets;
                }
                assign_idx = Some(idx);
                idx += 1;
                continue;
            }

            if assign_idx.is_none() {
                skip!(
                    "candidate-assign-not-reassign idx={} value={:?}",
                    idx,
                    instr.value
                );
            }
            let InstructionValue::Ternary {
                test,
                consequent,
                alternate,
                ..
            } = &instr.value
            else {
                skip!("expected-ternary idx={} value={:?}", idx, instr.value);
            };
            if test.identifier.declaration_id != test_decl_lvalue.identifier.declaration_id {
                skip!(
                    "ternary-test-decl-mismatch idx={} test={} expected={}",
                    idx,
                    test.identifier.declaration_id.0,
                    test_decl_lvalue.identifier.declaration_id.0
                );
            }
            ternary_idx = Some(idx);
            ternary_test = Some(test.clone());
            ternary_consequent = Some(consequent.clone());
            ternary_alternate = Some(alternate.clone());
            break;
        }
        if assign_idx.is_none() {
            if is_temp_bridge {
                idx += 1;
                continue;
            }
            skip!(
                "unexpected-before-assign idx={} value={:?}",
                idx,
                instr.value
            );
        }
        if is_temp_bridge {
            idx += 1;
            continue;
        }
        skip!(
            "unexpected-before-ternary idx={} value={:?}",
            idx,
            instr.value
        );
    }

    let assign_idx = assign_idx?;
    let ternary_idx = ternary_idx?;
    if assign_target_idents.is_empty() {
        return None;
    }
    let ternary_test = ternary_test?;
    let ternary_consequent = ternary_consequent?;
    let ternary_alternate = ternary_alternate?;

    // Materialize intermediary temporaries so consequent/alternate places are
    // renderable even when their builder instructions normally emit no output.
    let mut assign_stmt: Option<String> = None;
    for (offset, stmt) in block
        .iter()
        .take(ternary_idx + 1)
        .skip(start + 1)
        .enumerate()
    {
        let bridge_idx = start + 1 + offset;
        let ReactiveStatement::Instruction(instr) = stmt else {
            return None;
        };
        let emitted = codegen_instruction_nullable(&mut probe, instr);
        if let (Some(lvalue), Some(stmt)) = (&instr.lvalue, emitted.as_deref())
            && lvalue.identifier.name.is_some()
            && let Some(rhs) = extract_initializer_rhs_global(stmt)
        {
            named_bridge_expr_by_decl.insert(lvalue.identifier.declaration_id, rhs);
        }
        if bridge_idx == assign_idx {
            assign_stmt = emitted;
        }
    }

    let test_ev = codegen_instruction_value_ev(&mut probe, &test_decl_instr.value);
    let test_expr = if test_ev.prec <= ExprPrecedence::Assignment {
        format!("({})", test_ev.expr)
    } else {
        test_ev.expr
    };
    let assign_expr = extract_simple_expression_statement_global(assign_stmt.as_deref()?)?;
    let assign_target_names: Vec<String> = assign_target_idents
        .iter()
        .map(|ident| identifier_name_with_cx(&mut probe, ident))
        .collect();
    if assign_target_names
        .iter()
        .any(|target| contains_identifier_token(&test_expr, target))
    {
        return None;
    }

    let _ternary_test_expr =
        codegen_place_with_min_prec(&mut probe, &ternary_test, ExprPrecedence::Conditional);
    let mut consequent_expr =
        codegen_place_with_min_prec(&mut probe, &ternary_consequent, ExprPrecedence::Conditional);
    let mut alternate_expr =
        codegen_place_with_min_prec(&mut probe, &ternary_alternate, ExprPrecedence::Conditional);
    if let Some(inlined) =
        named_bridge_expr_by_decl.get(&ternary_consequent.identifier.declaration_id)
    {
        consequent_expr = inlined.clone();
    }
    if let Some(inlined) =
        named_bridge_expr_by_decl.get(&ternary_alternate.identifier.declaration_id)
    {
        alternate_expr = inlined.clone();
    }
    let consequent_uses_target = assign_target_names
        .iter()
        .any(|target| contains_identifier_token(&consequent_expr, target));
    let alternate_uses_target = assign_target_names
        .iter()
        .any(|target| contains_identifier_token(&alternate_expr, target));
    let fused_stmt = if consequent_uses_target != alternate_uses_target {
        if consequent_uses_target {
            format!(
                "{} ? (({}), {}) : {};\n",
                test_expr, assign_expr, consequent_expr, alternate_expr
            )
        } else {
            format!(
                "{} ? {} : (({}), {});\n",
                test_expr, consequent_expr, assign_expr, alternate_expr
            )
        }
    } else if consequent_uses_target && alternate_uses_target {
        let assign_trimmed = assign_expr.trim();
        let consequent_has_assign = consequent_expr.contains(assign_trimmed);
        let alternate_has_assign = alternate_expr.contains(assign_trimmed);
        if consequent_has_assign && !alternate_has_assign {
            format!(
                "{} ? {} : (({}), {});\n",
                test_expr, consequent_expr, assign_expr, alternate_expr
            )
        } else if alternate_has_assign && !consequent_has_assign {
            format!(
                "{} ? (({}), {}) : {};\n",
                test_expr, assign_expr, consequent_expr, alternate_expr
            )
        } else {
            return None;
        }
    } else {
        return None;
    };
    debug_codegen_expr(
        "fused-named-test-reassign-ternary-branch",
        format!(
            "temp={} targets={:?} test=`{}` assign=`{}` range={}..={}",
            temp_name, assign_target_names, test_expr, assign_expr, assign_idx, ternary_idx
        ),
    );
    output.push_str(
        &render_reactive_expression_statement_ast(fused_stmt.trim_end_matches(";\n"))
            .unwrap_or(fused_stmt),
    );
    Some(ternary_idx - start + 1)
}

fn maybe_codegen_fused_reassign_then_ternary_branch(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    #[derive(Clone)]
    struct CandidateAssign {
        idx: usize,
        expr: String,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BranchKind {
        Consequent,
        Alternate,
    }

    fn collect_named_reassign_targets(instr: &ReactiveInstruction) -> Option<Vec<Identifier>> {
        if instr.lvalue.is_some() {
            return None;
        }
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. }
                if lvalue.kind == InstructionKind::Reassign
                    && lvalue.place.identifier.name.is_some() =>
            {
                Some(vec![lvalue.place.identifier.clone()])
            }
            InstructionValue::Destructure { lvalue, .. }
                if lvalue.kind == InstructionKind::Reassign =>
            {
                let targets: Vec<Identifier> = pattern_operands(&lvalue.pattern)
                    .into_iter()
                    .filter(|operand| operand.identifier.name.is_some())
                    .map(|operand| operand.identifier.clone())
                    .collect();
                (!targets.is_empty()).then_some(targets)
            }
            _ => None,
        }
    }

    fn same_target_set(lhs: &[Identifier], rhs: &[Identifier]) -> bool {
        if lhs.len() != rhs.len() {
            return false;
        }
        let mut lhs_ids: Vec<u32> = lhs.iter().map(|ident| ident.declaration_id.0).collect();
        let mut rhs_ids: Vec<u32> = rhs.iter().map(|ident| ident.declaration_id.0).collect();
        lhs_ids.sort_unstable();
        rhs_ids.sort_unstable();
        lhs_ids == rhs_ids
    }

    fn last_assignment_before(
        assignments: &[CandidateAssign],
        lower_bound: usize,
        upper_bound: usize,
    ) -> Option<&CandidateAssign> {
        assignments
            .iter()
            .rev()
            .find(|assign| assign.idx > lower_bound && assign.idx < upper_bound)
    }

    fn maybe_fuse_branch_expr(
        branch_expr: String,
        uses_target: bool,
        assign_expr: Option<&str>,
    ) -> String {
        if !uses_target {
            return branch_expr;
        }
        let Some(assign_expr) = assign_expr else {
            return branch_expr;
        };
        let assign_trimmed = assign_expr.trim();
        if !assign_trimmed.is_empty() && branch_expr.contains(assign_trimmed) {
            branch_expr
        } else {
            format!("(({}), {})", assign_expr, branch_expr)
        }
    }

    let ReactiveStatement::Instruction(first_assign_instr) = block.get(start)? else {
        return None;
    };
    let target_idents = collect_named_reassign_targets(first_assign_instr)?;
    let mut probe_cx = cx.clone();

    let mut assignments: Vec<CandidateAssign> = Vec::new();
    let mut lvalue_indices: HashMap<DeclarationId, usize> = HashMap::new();
    let mut ternary_idx: Option<usize> = None;
    let mut ternary_operands: Option<(Place, Place, Place)> = None;

    let mut idx = start;
    while idx < block.len() {
        let ReactiveStatement::Instruction(instr) = &block[idx] else {
            return None;
        };

        if instr.lvalue.is_none() {
            if let InstructionValue::Ternary {
                test,
                consequent,
                alternate,
                ..
            } = &instr.value
            {
                ternary_idx = Some(idx);
                ternary_operands = Some((test.clone(), consequent.clone(), alternate.clone()));
                break;
            }

            let targets = collect_named_reassign_targets(instr)?;
            if !same_target_set(&target_idents, &targets) {
                return None;
            }
            let stmt = codegen_instruction_nullable(&mut probe_cx, instr)?;
            let expr = extract_simple_expression_statement_global(&stmt)?;
            assignments.push(CandidateAssign { idx, expr });
            idx += 1;
            continue;
        }

        let lvalue = instr.lvalue.as_ref().expect("checked above");
        lvalue_indices.insert(lvalue.identifier.declaration_id, idx);
        if !is_temp_like_identifier(&probe_cx, &lvalue.identifier) {
            return None;
        }
        if codegen_instruction_nullable(&mut probe_cx, instr).is_some() {
            return None;
        }
        idx += 1;
    }

    let ternary_idx = ternary_idx?;
    let (test, consequent, alternate) = ternary_operands?;

    let target_names: Vec<String> = target_idents
        .iter()
        .map(|ident| identifier_name_with_cx(&mut probe_cx, ident))
        .collect();
    let test_expr = codegen_place_with_min_prec(&mut probe_cx, &test, ExprPrecedence::Conditional);
    if target_names
        .iter()
        .any(|target| contains_identifier_token(&test_expr, target))
    {
        return None;
    }

    let consequent_expr =
        codegen_place_with_min_prec(&mut probe_cx, &consequent, ExprPrecedence::Conditional);
    let alternate_expr =
        codegen_place_with_min_prec(&mut probe_cx, &alternate, ExprPrecedence::Conditional);
    let consequent_uses_target = target_names
        .iter()
        .any(|target| contains_identifier_token(&consequent_expr, target));
    let alternate_uses_target = target_names
        .iter()
        .any(|target| contains_identifier_token(&alternate_expr, target));
    if !consequent_uses_target && !alternate_uses_target {
        return None;
    }

    let consequent_result_idx = lvalue_indices
        .get(&consequent.identifier.declaration_id)
        .copied()
        .unwrap_or(ternary_idx);
    let alternate_result_idx = lvalue_indices
        .get(&alternate.identifier.declaration_id)
        .copied()
        .unwrap_or(ternary_idx);

    let mut branch_ranges = vec![
        (BranchKind::Consequent, consequent_result_idx),
        (BranchKind::Alternate, alternate_result_idx),
    ];
    branch_ranges.sort_by_key(|(_, idx)| *idx);

    let mut lower_bound = start.saturating_sub(1);
    let mut consequent_assign: Option<&str> = None;
    let mut alternate_assign: Option<&str> = None;
    for (branch, result_idx) in branch_ranges {
        let assign = last_assignment_before(&assignments, lower_bound, result_idx)
            .map(|candidate| candidate.expr.as_str());
        match branch {
            BranchKind::Consequent => consequent_assign = assign,
            BranchKind::Alternate => alternate_assign = assign,
        }
        lower_bound = result_idx;
    }

    let fused_consequent = maybe_fuse_branch_expr(
        consequent_expr.clone(),
        consequent_uses_target,
        consequent_assign,
    );
    let fused_alternate = maybe_fuse_branch_expr(
        alternate_expr.clone(),
        alternate_uses_target,
        alternate_assign,
    );
    if fused_consequent == consequent_expr && fused_alternate == alternate_expr {
        return None;
    }

    let fused_stmt = format!("{test_expr} ? {fused_consequent} : {fused_alternate}");
    debug_codegen_expr(
        "fused-reassign-ternary-branch",
        format!(
            "targets={:?} cons_assign={:?} alt_assign={:?} test=`{}` range={}..={}",
            target_names, consequent_assign, alternate_assign, test_expr, start, ternary_idx
        ),
    );
    output.push_str(
        &render_reactive_expression_statement_ast(&fused_stmt)
            .unwrap_or_else(|| format!("{fused_stmt};\n")),
    );
    *cx = probe_cx;
    Some(ternary_idx - start + 1)
}

fn maybe_codegen_fused_reassign_temp_load_then_ternary(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(store_instr) = block.get(start)? else {
        return None;
    };
    let temp_lvalue = store_instr.lvalue.as_ref()?;
    if !is_temp_like_identifier(cx, &temp_lvalue.identifier) {
        return None;
    }

    let reassigned_target = match &store_instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
            if lvalue.kind == InstructionKind::Reassign =>
        {
            lvalue.place.identifier.declaration_id
        }
        _ => return None,
    };

    let ReactiveStatement::Instruction(assign_load_instr) = block.get(start + 1)? else {
        return None;
    };
    if assign_load_instr.lvalue.is_some() {
        return None;
    }
    match &assign_load_instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. }
            if place.identifier.declaration_id == temp_lvalue.identifier.declaration_id => {}
        _ => return None,
    };

    let mut probe_cx = cx.clone();
    if codegen_instruction_nullable(&mut probe_cx, store_instr).is_some() {
        return None;
    }
    let assign_stmt = codegen_instruction_nullable(&mut probe_cx, assign_load_instr)?;
    let assign_expr = extract_simple_expression_statement_global(&assign_stmt)?;

    let mut idx = start + 2;
    while idx < block.len() {
        let ReactiveStatement::Instruction(instr) = block.get(idx)? else {
            return None;
        };

        if instr.lvalue.is_none()
            && let InstructionValue::Ternary {
                consequent,
                alternate,
                ..
            } = &instr.value
        {
            let uses_reassign_temp = consequent.identifier.declaration_id
                == temp_lvalue.identifier.declaration_id
                || alternate.identifier.declaration_id == temp_lvalue.identifier.declaration_id;
            let uses_reassign_target = consequent.identifier.declaration_id == reassigned_target
                || alternate.identifier.declaration_id == reassigned_target;
            if !uses_reassign_temp && !uses_reassign_target {
                return None;
            }

            let ternary_stmt = codegen_instruction_nullable(&mut probe_cx, instr)?;
            let ternary_expr = extract_simple_expression_statement_global(&ternary_stmt)?;
            if !ternary_expr.contains(assign_expr.trim()) {
                return None;
            }

            debug_codegen_expr(
                "fused-reassign-temp-load-ternary",
                format!(
                    "temp_decl={} target_decl={} range={}..={} assign=`{}`",
                    temp_lvalue.identifier.declaration_id.0,
                    reassigned_target.0,
                    start,
                    idx,
                    assign_expr
                ),
            );
            output.push_str(&ternary_stmt);
            return Some(idx - start + 1);
        }

        if codegen_instruction_nullable(&mut probe_cx, instr).is_some() {
            return None;
        }
        idx += 1;
    }

    None
}

fn maybe_codegen_fused_temp_load_into_following_stmt(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(load_instr) = block.get(start)? else {
        return None;
    };
    if load_instr.lvalue.is_some() {
        return None;
    }
    let source_place = match &load_instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            place
        }
        _ => return None,
    };
    let load_start_line = reactive_instruction_start_line(load_instr);
    let requires_same_line_consumer = source_place.identifier.name.is_some();

    let mut probe_cx = cx.clone();
    let temp_stmt = codegen_instruction_nullable(&mut probe_cx, load_instr)?;
    let mut bridge_exprs = vec![extract_simple_expression_statement_global(&temp_stmt)?];

    // Flattened value-block lowering can push the real consumer statement far
    // beyond the immediate bridge loads (for example nested logical/ternary
    // chains that only materialize at a later named store/destructure). Keep
    // scanning until the first emitted statement instead of bailing out after
    // an arbitrary short window.
    for idx in (start + 1)..block.len() {
        match block.get(idx)? {
            ReactiveStatement::Instruction(instr) => {
                let Some(stmt) = codegen_instruction_nullable(&mut probe_cx, instr) else {
                    continue;
                };

                let is_bridge_stmt = instr.lvalue.is_none()
                    && matches!(
                        &instr.value,
                        InstructionValue::LoadLocal { place, .. }
                            | InstructionValue::LoadContext { place, .. }
                            if place.identifier.name.is_none()
                    );
                if is_bridge_stmt
                    && let Some(expr) = extract_simple_expression_statement_global(&stmt)
                {
                    bridge_exprs.push(expr);
                    continue;
                }

                let contains_all_bridge_exprs = extract_simple_expression_statement_global(&stmt)
                    .is_some_and(|expr| contains_all_bridge_exprs_normalized(&expr, &bridge_exprs))
                    || extract_initializer_rhs_global(&stmt).is_some_and(|rhs| {
                        contains_all_bridge_exprs_normalized(&rhs, &bridge_exprs)
                    });
                if !contains_all_bridge_exprs {
                    return None;
                }
                if requires_same_line_consumer
                    && load_start_line != reactive_instruction_start_line(instr)
                {
                    return None;
                }

                debug_codegen_expr(
                    "fused-temp-load-following-stmt",
                    format!(
                        "temp_decl={} range={}..={} exprs={:?}",
                        source_place.identifier.declaration_id.0, start, idx, bridge_exprs
                    ),
                );
                output.push_str(&stmt);
                *cx = probe_cx;
                return Some(idx - start + 1);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let stmt = codegen_terminal(&mut probe_cx, &term_stmt.terminal)?;
                let contains_all_bridge_exprs =
                    contains_all_bridge_exprs_normalized(&stmt, &bridge_exprs);
                if !contains_all_bridge_exprs {
                    return None;
                }
                if requires_same_line_consumer
                    && load_start_line != reactive_terminal_start_line(&term_stmt.terminal)
                {
                    return None;
                }

                debug_codegen_expr(
                    "fused-temp-load-following-terminal",
                    format!(
                        "temp_decl={} range={}..={} exprs={:?}",
                        source_place.identifier.declaration_id.0, start, idx, bridge_exprs
                    ),
                );
                if let Some(label) = &term_stmt.label {
                    if !label.implicit {
                        emit_labeled_statement(output, label.id, &stmt);
                    } else {
                        output.push_str(&stmt);
                    }
                } else {
                    output.push_str(&stmt);
                }
                if !output.ends_with('\n') {
                    output.push('\n');
                }
                *cx = probe_cx;
                return Some(idx - start + 1);
            }
            ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_) => return None,
        }
    }

    None
}

fn maybe_codegen_fused_reassign_stmt_into_following_logical(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    if first_instr.lvalue.is_some() {
        return None;
    }
    let (target_ident, value_place) = match &first_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. }
            if lvalue.kind == InstructionKind::Reassign
                && lvalue.place.identifier.name.is_some() =>
        {
            (&lvalue.place.identifier, value)
        }
        _ => return None,
    };
    if value_place.identifier.name.is_some() {
        return None;
    }
    let start_line = reactive_instruction_start_line(first_instr);
    let mut probe_cx = cx.clone();
    let assign_stmt = codegen_instruction_nullable(&mut probe_cx, first_instr)?;
    let assign_expr = extract_simple_expression_statement_global(&assign_stmt)?;
    let assign_rhs = assign_expr.split_once(" = ")?.1.trim().to_string();
    let mut bridge_exprs: Vec<String> = Vec::new();

    for idx in (start + 1)..block.len() {
        match block.get(idx)? {
            ReactiveStatement::Instruction(instr) => {
                let Some(stmt) = codegen_instruction_nullable(&mut probe_cx, instr) else {
                    continue;
                };
                let is_bridge_stmt = instr.lvalue.is_none()
                    && matches!(
                        &instr.value,
                        InstructionValue::LoadLocal { place, .. }
                            | InstructionValue::LoadContext { place, .. }
                            if place.identifier.name.is_none()
                    );
                if is_bridge_stmt
                    && let Some(expr) = extract_simple_expression_statement_global(&stmt)
                {
                    bridge_exprs.push(expr);
                    continue;
                }

                let InstructionValue::LogicalExpression {
                    operator,
                    left,
                    right,
                    ..
                } = &instr.value
                else {
                    return None;
                };
                if instr.lvalue.is_some() || start_line != reactive_instruction_start_line(instr) {
                    return None;
                }
                if !extract_simple_expression_statement_global(&stmt)
                    .is_some_and(|expr| contains_all_bridge_exprs_normalized(&expr, &bridge_exprs))
                {
                    return None;
                }
                let logical_prec = logical_operator_precedence(operator);
                let left_expr = codegen_logical_operand(&mut probe_cx, left, logical_prec);
                let right_expr = codegen_logical_operand(&mut probe_cx, right, logical_prec);
                let combined_expr = if normalize_fusion_match_text(&right_expr) == "null" {
                    if normalize_fusion_match_text(&assign_rhs)
                        == normalize_fusion_match_text(&left_expr)
                    {
                        format!("({assign_expr}) {} null", logical_operator_to_str(operator))
                    } else {
                        format!(
                            "{} {} (({}), null)",
                            left_expr,
                            logical_operator_to_str(operator),
                            assign_expr
                        )
                    }
                } else {
                    let combined_left = if normalize_fusion_match_text(&assign_rhs)
                        == normalize_fusion_match_text(&left_expr)
                    {
                        format!("({assign_expr})")
                    } else {
                        format!("(({assign_expr}), {left_expr})")
                    };
                    format!(
                        "{} {} {}",
                        combined_left,
                        logical_operator_to_str(operator),
                        right_expr
                    )
                };
                debug_codegen_expr(
                    "fused-reassign-following-logical",
                    format!(
                        "target={} range={}..={} bridges={:?}",
                        identifier_name_with_cx(&mut probe_cx, target_ident),
                        start,
                        idx,
                        bridge_exprs
                    ),
                );
                output.push_str(
                    &render_reactive_expression_statement_ast(&combined_expr)
                        .unwrap_or_else(|| format!("{combined_expr};\n")),
                );
                *cx = probe_cx;
                return Some(idx - start + 1);
            }
            ReactiveStatement::Terminal(_) => return None,
            ReactiveStatement::Scope(_) | ReactiveStatement::PrunedScope(_) => return None,
        }
    }

    None
}

fn maybe_codegen_fused_reassign_stmt_into_following_null(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    if first_instr.lvalue.is_some() {
        return None;
    }
    match &first_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. }
            if lvalue.kind == InstructionKind::Reassign
                && lvalue.place.identifier.name.is_some()
                && value.identifier.name.is_none() => {}
        _ => return None,
    }
    let ReactiveStatement::Instruction(null_instr) = block.get(start + 1)? else {
        return None;
    };
    if null_instr.lvalue.is_some()
        || !matches!(
            null_instr.value,
            InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                ..
            }
        )
    {
        return None;
    }

    let stmt = codegen_instruction_nullable(cx, first_instr)?;
    output.push_str(&stmt);
    if !stmt.ends_with('\n') {
        output.push('\n');
    }
    debug_codegen_expr(
        "fused-reassign-following-null",
        format!("start={start} target-only"),
    );
    Some(2)
}

fn maybe_codegen_fused_pruned_scope_prefix_into_following_stmt(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let debug_pruned_fusion = std::env::var("DEBUG_PRUNED_FUSION").is_ok();
    let ReactiveStatement::PrunedScope(pruned) = block.get(start)? else {
        return None;
    };
    let ReactiveStatement::Instruction(next_instr) = block.get(start + 1)? else {
        return None;
    };

    let mut probe_cx = cx.clone();
    let inner = codegen_block_no_reset(&mut probe_cx, &pruned.instructions);
    let next_stmt = codegen_instruction_nullable(&mut probe_cx, next_instr)?;
    let next_stmt_expr = extract_simple_expression_statement_global(&next_stmt);
    let next_stmt_rhs = extract_initializer_rhs_global(&next_stmt);
    let next_stmt_contains_all = |bridge_exprs: &[String]| {
        next_stmt_expr
            .as_ref()
            .is_some_and(|expr| contains_all_bridge_exprs_normalized(expr, bridge_exprs))
            || next_stmt_rhs
                .as_ref()
                .is_some_and(|rhs| contains_all_bridge_exprs_normalized(rhs, bridge_exprs))
    };
    let Some(chunks) = split_top_level_statement_chunks_global(&inner) else {
        if debug_pruned_fusion {
            eprintln!(
                "[PRUNED_FUSION] start={} no-chunks inner={:?}",
                start, inner
            );
        }
        return None;
    };
    let mut split_idx = chunks.len();
    let mut bridge_exprs = Vec::new();
    while split_idx > 0 {
        let Some(expr) = extract_simple_expression_statement_global(&chunks[split_idx - 1]) else {
            break;
        };
        let mut candidate = vec![expr];
        candidate.extend(bridge_exprs.iter().cloned());
        if !next_stmt_contains_all(&candidate) {
            break;
        }
        bridge_exprs = candidate;
        split_idx -= 1;
    }
    if bridge_exprs.is_empty() {
        if debug_pruned_fusion {
            eprintln!(
                "[PRUNED_FUSION] start={} no-subsumed-bridge inner={:?} next_stmt={:?} next_expr={:?} next_rhs={:?}",
                start, inner, next_stmt, next_stmt_expr, next_stmt_rhs
            );
        }
        return None;
    }
    let mut prefix_code = String::new();
    for chunk in &chunks[..split_idx] {
        prefix_code.push_str(chunk);
        if !prefix_code.ends_with('\n') {
            prefix_code.push('\n');
        }
    }
    let contains_all_bridge_exprs = next_stmt_contains_all(&bridge_exprs);
    if debug_pruned_fusion {
        eprintln!(
            "[PRUNED_FUSION] start={} bridge_exprs={:?} next_stmt={:?} next_expr={:?} next_rhs={:?} contains={}",
            start,
            bridge_exprs,
            next_stmt,
            next_stmt_expr,
            next_stmt_rhs,
            contains_all_bridge_exprs
        );
    }
    if !contains_all_bridge_exprs {
        return None;
    }

    debug_codegen_expr(
        "fused-pruned-scope-prefix-following-stmt",
        format!(
            "start={} prefix_lines={} exprs={:?}",
            start,
            prefix_code.lines().count(),
            bridge_exprs
                .iter()
                .map(|expr| expr.trim().to_string())
                .collect::<Vec<_>>()
        ),
    );
    output.push_str(&prefix_code);
    output.push_str(&next_stmt);
    *cx = probe_cx;
    Some(2)
}

fn maybe_codegen_fused_named_temp_logical_expression(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    let first_lvalue = first_instr.lvalue.as_ref()?;
    let first_name = first_lvalue.identifier.name.as_ref()?.value();
    if !is_codegen_temp_name(first_name) {
        return None;
    }

    let target_decl = first_lvalue.identifier.declaration_id;
    let mut probe_cx = cx.clone();
    let mut expr_by_identifier: HashMap<IdentifierId, ExprValue> = HashMap::new();
    let mut saw_named_bridge = false;
    let mut idx = start;
    loop {
        let ReactiveStatement::Instruction(instr) = block.get(idx)? else {
            return None;
        };

        if let Some(lvalue) = &instr.lvalue {
            let same_named_target = lvalue.identifier.declaration_id == target_decl
                && lvalue
                    .identifier
                    .name
                    .as_ref()
                    .is_some_and(|name| name.value() == first_name);

            if same_named_target {
                if let InstructionValue::LogicalExpression {
                    operator,
                    left,
                    right,
                    ..
                } = &instr.value
                {
                    if !saw_named_bridge {
                        return None;
                    }
                    let left_ev = expr_by_identifier
                        .get(&left.identifier.id)
                        .cloned()
                        .unwrap_or_else(|| codegen_place_expr_value(&mut probe_cx, left));
                    let right_ev = expr_by_identifier
                        .get(&right.identifier.id)
                        .cloned()
                        .unwrap_or_else(|| codegen_place_expr_value(&mut probe_cx, right));
                    let logical_prec = logical_operator_precedence(operator);
                    let left_expr = codegen_logical_operand_from_expr_value(left_ev, logical_prec);
                    let right_expr =
                        codegen_logical_operand_from_expr_value(right_ev, logical_prec);
                    let target_name = identifier_name_with_cx(&mut probe_cx, &lvalue.identifier);
                    let op = logical_operator_to_str(operator);
                    let logical_expr = format!("{} {} {}", left_expr, op, right_expr);

                    if has_materialized_named_binding(&probe_cx, &lvalue.identifier) {
                        output.push_str(
                            &render_reactive_assignment_statement_ast(&target_name, &logical_expr)
                                .unwrap_or_else(|| format!("{} = {};\n", target_name, logical_expr)),
                        );
                    } else {
                        probe_cx.declare(&lvalue.identifier);
                        probe_cx.mark_decl_runtime_emitted(lvalue.identifier.declaration_id);
                        output.push_str(
                            &render_reactive_variable_statement_ast(
                                ast::VariableDeclarationKind::Const,
                                &target_name,
                                Some(&logical_expr),
                            )
                            .unwrap_or_else(|| format!("const {} = {};\n", target_name, logical_expr)),
                        );
                    }
                    probe_cx.set_temp_expr(&lvalue.identifier, None);
                    *cx = probe_cx;
                    return Some(idx - start + 1);
                }

                let rhs_ev = codegen_instruction_value_ev(&mut probe_cx, &instr.value);
                expr_by_identifier.insert(lvalue.identifier.id, rhs_ev);
                saw_named_bridge = true;
                idx += 1;
                continue;
            }
        }

        if reactive_instruction_uses_declaration(instr, target_decl) {
            return None;
        }

        let emitted = codegen_instruction_nullable(&mut probe_cx, instr);
        if emitted
            .as_deref()
            .is_some_and(|stmt| !stmt.trim().is_empty())
        {
            return None;
        }
        idx += 1;
    }
}

fn maybe_codegen_fused_named_temp_ternary_statement(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    let first_lvalue = first_instr.lvalue.as_ref()?;
    let first_name = first_lvalue.identifier.name.as_ref()?.value();
    if !is_codegen_temp_name(first_name)
        || has_materialized_named_binding(cx, &first_lvalue.identifier)
    {
        return None;
    }

    let mut probe_cx = cx.clone();
    let mut inlined_decls: Vec<DeclarationId> = Vec::new();
    let mut inlined_names: Vec<String> = Vec::new();
    let mut idx = start;
    loop {
        let ReactiveStatement::Instruction(instr) = block.get(idx)? else {
            return None;
        };

        if let Some(lvalue) = &instr.lvalue
            && let Some(name) = lvalue.identifier.name.as_ref().map(IdentifierName::value)
            && is_codegen_temp_name(name)
            && !has_materialized_named_binding(&probe_cx, &lvalue.identifier)
        {
            let ev = codegen_instruction_value_ev(&mut probe_cx, &instr.value);
            probe_cx.set_temp_expr(&lvalue.identifier, Some(ev));
            inlined_decls.push(lvalue.identifier.declaration_id);
            inlined_names.push(name.to_string());
            idx += 1;
            continue;
        }

        let InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } = &instr.value
        else {
            let emitted = codegen_instruction_nullable(&mut probe_cx, instr);
            if emitted
                .as_deref()
                .is_some_and(|stmt| !stmt.trim().is_empty())
            {
                return None;
            }
            idx += 1;
            continue;
        };
        if instr.lvalue.is_some() {
            return None;
        }

        let uses_inlined_decl = [test, consequent, alternate]
            .iter()
            .any(|place| inlined_decls.contains(&place.identifier.declaration_id));
        if !uses_inlined_decl {
            return None;
        }
        if inlined_decls.iter().any(|decl_id| {
            reactive_block_uses_declaration(&block[idx + 1..], *decl_id)
                || reactive_block_writes_declaration(&block[idx + 1..], *decl_id)
        }) {
            return None;
        }

        let stmt = codegen_instruction_nullable(&mut probe_cx, instr)?;
        if inlined_names
            .iter()
            .any(|name| contains_identifier_token(&stmt, name))
        {
            return None;
        }
        debug_codegen_expr(
            "fused-named-temp-ternary-statement",
            format!("start={} end={} decls={:?}", start, idx, inlined_names),
        );
        output.push_str(&stmt);
        *cx = probe_cx;
        return Some(idx - start + 1);
    }
}

fn maybe_codegen_fused_method_call_eval_order(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    macro_rules! skip {
        ($($arg:tt)*) => {{
            debug_codegen_expr(
                "fused-method-call-eval-order-skip",
                format!("start={} {}", start, format!($($arg)*)),
            );
            return None;
        }};
    }

    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    if !is_eval_order_fusion_participant(first_instr) {
        return None;
    }
    if start > 0
        && let ReactiveStatement::Instruction(prev_instr) = &block[start - 1]
        && is_eval_order_fusion_participant(prev_instr)
    {
        return None;
    }

    let Some(participants) = collect_eval_order_fusion_participants(block, start) else {
        skip!("collect-participants-failed");
    };
    if participants.len() <= 1 {
        skip!("participants-too-short len={}", participants.len());
    }
    let final_idx = participants.len() - 1;
    let final_instr = participants[final_idx].instr;
    let InstructionValue::MethodCall {
        receiver,
        property,
        args,
        receiver_optional,
        call_optional,
        ..
    } = &final_instr.value
    else {
        skip!("final-not-method-call kind={:?}", final_instr.value);
    };
    if final_instr.lvalue.is_some() {
        skip!("final-has-lvalue");
    }

    let has_prefix_side_effect = participants[..final_idx]
        .iter()
        .any(|entry| is_side_effect_call_instruction(entry.instr));
    if !has_prefix_side_effect {
        skip!("no-prefix-side-effect");
    }

    let mut def_idx_by_decl: HashMap<DeclarationId, usize> = HashMap::new();
    for (idx, entry) in participants[..final_idx].iter().enumerate() {
        let instr = entry.instr;
        if let Some(lvalue) = &instr.lvalue {
            def_idx_by_decl.insert(lvalue.identifier.declaration_id, idx);
        }
    }

    let Some(receiver_def_idx) = def_idx_by_decl
        .get(&receiver.identifier.declaration_id)
        .copied()
    else {
        skip!(
            "missing-receiver-def decl={}",
            receiver.identifier.declaration_id.0
        );
    };
    let Some(property_def_idx) = def_idx_by_decl
        .get(&property.identifier.declaration_id)
        .copied()
    else {
        skip!(
            "missing-property-def decl={}",
            property.identifier.declaration_id.0
        );
    };
    let mut arg_def_indices = Vec::with_capacity(args.len());
    for arg in args {
        let decl_id = match arg {
            Argument::Place(place) | Argument::Spread(place) => place.identifier.declaration_id,
        };
        let Some(arg_def_idx) = def_idx_by_decl.get(&decl_id).copied() else {
            skip!("missing-arg-def decl={}", decl_id.0);
        };
        arg_def_indices.push(arg_def_idx);
    }

    let mut ordered_defs = Vec::with_capacity(2 + arg_def_indices.len());
    ordered_defs.push(receiver_def_idx);
    ordered_defs.push(property_def_idx);
    ordered_defs.extend(arg_def_indices.iter().copied());
    if !ordered_defs.windows(2).all(|pair| pair[0] <= pair[1]) {
        skip!("unordered-defs={:?}", ordered_defs);
    }

    let collect_prefix_side_effect_indices = |from: usize, to: usize| -> Option<Vec<usize>> {
        let participants = participants.get(from..to)?;
        let mut indices = Vec::new();
        for (offset, participant) in participants.iter().enumerate() {
            let idx = from + offset;
            let instr = participant.instr;
            if is_side_effect_call_instruction(instr) {
                indices.push(idx);
            } else if instr.lvalue.is_some()
                || is_ignorable_eval_order_fusion_temp_declaration(instr)
                || is_ignorable_eval_order_fusion_interstitial(instr)
            {
                continue;
            } else {
                return None;
            }
        }
        Some(indices)
    };

    let Some(receiver_prefix_indices) = collect_prefix_side_effect_indices(start, receiver_def_idx)
    else {
        skip!("collect-receiver-prefix-failed");
    };
    let Some(property_prefix_indices) =
        collect_prefix_side_effect_indices(receiver_def_idx + 1, property_def_idx)
    else {
        skip!("collect-property-prefix-failed");
    };

    let mut arg_prefix_indices = vec![Vec::new(); args.len()];
    let mut range_start = property_def_idx + 1;
    for (arg_idx, arg_def_idx) in arg_def_indices.iter().enumerate() {
        let Some(collected) = collect_prefix_side_effect_indices(range_start, *arg_def_idx) else {
            skip!("collect-arg-prefix-failed arg_idx={}", arg_idx);
        };
        arg_prefix_indices[arg_idx] = collected;
        range_start = *arg_def_idx + 1;
    }

    let Some(trailing_prefix_indices) = collect_prefix_side_effect_indices(range_start, final_idx)
    else {
        skip!("collect-trailing-prefix-failed");
    };
    if !trailing_prefix_indices.is_empty() {
        skip!("trailing-prefix-not-empty {:?}", trailing_prefix_indices);
    }

    // When only receiver-prefix side effects exist, emitting comma-sequence receiver
    // expressions (e.g. `(x.push(a), x).push(b)`) diverges from upstream, which keeps
    // these as separate statements. Let default emission handle this case.
    if !receiver_prefix_indices.is_empty()
        && property_prefix_indices.is_empty()
        && arg_prefix_indices.iter().all(Vec::is_empty)
    {
        skip!("receiver-only-prefixes");
    }

    if receiver_prefix_indices.is_empty()
        && property_prefix_indices.is_empty()
        && arg_prefix_indices.iter().all(Vec::is_empty)
    {
        skip!("no-prefixes");
    }

    let consumed_top_level = participants[final_idx].top_level_idx - start + 1;
    let tail_start = start + consumed_top_level;
    for idx in 0..final_idx {
        let instr = participants[idx].instr;
        if is_ignorable_eval_order_fusion_temp_declaration(instr) {
            if can_ignore_eval_order_fusion_temp_declaration(block, tail_start, &participants, idx)
            {
                continue;
            }
            skip!(
                "non-droppable-ignorable-temp idx={} value={:?}",
                idx,
                instr.value
            );
        }
        if instr.lvalue.is_none() {
            continue;
        }
        if materialize_fusable_temp_instruction(cx, instr) {
            continue;
        }
        skip!(
            "cannot-materialize idx={} value={:?} lvalue={:?}",
            idx,
            instr.value,
            instr.lvalue
        );
    }

    let recv_prefix_exprs = render_side_effect_prefix_exprs_from_participants(
        cx,
        &participants,
        &receiver_prefix_indices,
    )?;
    let recv_base = codegen_place_to_expression(cx, receiver);
    let mut recv = recv_base.clone();
    recv = wrap_sequence_expr(&recv_prefix_exprs, recv);

    let property_prefix_exprs = render_side_effect_prefix_exprs_from_participants(
        cx,
        &participants,
        &property_prefix_indices,
    )?;
    let (resolved_prop, resolved_is_computed) = resolve_method_property(cx, property, &recv_base);

    let mut rendered_args = Vec::with_capacity(args.len());
    for (arg_idx, arg) in args.iter().enumerate() {
        let prefix_exprs = render_side_effect_prefix_exprs_from_participants(
            cx,
            &participants,
            &arg_prefix_indices[arg_idx],
        )?;
        let arg_expr = codegen_argument(cx, arg);
        rendered_args.push(wrap_sequence_expr(&prefix_exprs, arg_expr));
    }

    let (is_computed, prop, hook_name_owned) = if !property_prefix_indices.is_empty() {
        let property_tail = if resolved_is_computed {
            resolved_prop.clone()
        } else {
            format!("\"{}\"", escape_string(&resolved_prop))
        };
        let property_sequence_expr = wrap_sequence_expr(&property_prefix_exprs, property_tail);
        let hook = if resolved_is_computed {
            None
        } else {
            Some(resolved_prop.clone())
        };
        (true, property_sequence_expr, hook)
    } else {
        // Resolve static-vs-computed against the original receiver expression.
        // `recv` may be sequence-wrapped (e.g. `(console.log("A"), x)`), and
        // using that string here can incorrectly force computed form (`[x.f]`).
        if resolved_is_computed {
            (true, resolved_prop, None)
        } else {
            let hook = resolved_prop.clone();
            (false, resolved_prop, Some(hook))
        }
    };
    let hook_name = hook_name_owned.as_deref();

    maybe_replace_autodeps_with_inferred_deps(
        cx,
        hook_name.unwrap_or(""),
        args,
        &mut rendered_args,
        hook_name.is_some(),
    );
    let args_str = join_call_arguments(&rendered_args);

    let call_expr = if is_computed {
        let opt_recv = if *receiver_optional { "?." } else { "" };
        if *call_optional {
            format!("{}{}[{}]?.({})", recv, opt_recv, prop, args_str)
        } else {
            format!("{}{}[{}]({})", recv, opt_recv, prop, args_str)
        }
    } else {
        let dot = if *receiver_optional { "?." } else { "." };
        if *call_optional {
            format!("{}{}{}?.({})", recv, dot, prop, args_str)
        } else if !*receiver_optional
            && recv.starts_with("new ")
            && (recv.contains('\n') || prop == "build" || recv.matches('.').count() >= 1)
        {
            format!("{}\n.{}({})", recv, prop, args_str)
        } else {
            format!("{}{}{}({})", recv, dot, prop, args_str)
        }
    };
    let call_expr = if cx.emit_hook_guards && hook_name.is_some_and(Environment::is_hook_name) {
        wrap_hook_guarded_call_expression(&call_expr)
    } else {
        call_expr
    };

    debug_codegen_expr(
        "fused-method-call-eval-order",
        format!(
            "start={} end={} recv_prefix={} prop_prefix={} arg_prefixes={:?} consumed_top_level={}",
            start,
            final_idx,
            receiver_prefix_indices.len(),
            property_prefix_indices.len(),
            arg_prefix_indices
                .iter()
                .map(std::vec::Vec::len)
                .collect::<Vec<_>>(),
            consumed_top_level
        ),
    );
    output.push_str(
        &render_reactive_expression_statement_ast(&call_expr)
            .unwrap_or_else(|| format!("{call_expr};\n")),
    );
    Some(consumed_top_level)
}

struct EvalOrderFusionParticipant<'a> {
    instr: &'a ReactiveInstruction,
    top_level_idx: usize,
}

fn collect_eval_order_fusion_participants<'a>(
    block: &'a [ReactiveStatement],
    start: usize,
) -> Option<Vec<EvalOrderFusionParticipant<'a>>> {
    let mut participants = Vec::new();
    let mut top_level_idx = start;
    while top_level_idx < block.len() {
        match &block[top_level_idx] {
            ReactiveStatement::Instruction(instr) => {
                if !is_eval_order_fusion_participant(instr)
                    && !is_ignorable_eval_order_fusion_temp_declaration(instr)
                    && !is_ignorable_eval_order_fusion_interstitial(instr)
                {
                    break;
                }
                participants.push(EvalOrderFusionParticipant {
                    instr,
                    top_level_idx,
                });
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                let mut accepted_any = false;
                for stmt in &scope_block.instructions {
                    let ReactiveStatement::Instruction(instr) = stmt else {
                        debug_codegen_expr(
                            "fused-method-call-eval-order-skip",
                            format!(
                                "start={} pruned-scope-non-instruction top_idx={}",
                                start, top_level_idx
                            ),
                        );
                        return None;
                    };
                    if is_eval_order_fusion_participant(instr)
                        || is_ignorable_eval_order_fusion_temp_declaration(instr)
                        || is_ignorable_eval_order_fusion_interstitial(instr)
                    {
                        participants.push(EvalOrderFusionParticipant {
                            instr,
                            top_level_idx,
                        });
                        accepted_any = true;
                        continue;
                    }
                    debug_codegen_expr(
                        "fused-method-call-eval-order-skip",
                        format!(
                            "start={} pruned-scope-reject top_idx={} value={:?} lvalue={:?}",
                            start, top_level_idx, instr.value, instr.lvalue
                        ),
                    );
                    return None;
                }
                if !accepted_any {
                    break;
                }
            }
            _ => break,
        }
        top_level_idx += 1;
    }
    Some(participants)
}

fn is_ignorable_eval_order_fusion_temp_declaration(instr: &ReactiveInstruction) -> bool {
    match &instr.value {
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => is_fusable_temp_lvalue(&lvalue.place),
        _ => false,
    }
}

fn is_ignorable_eval_order_fusion_interstitial(instr: &ReactiveInstruction) -> bool {
    if instr.lvalue.is_some() {
        return false;
    }
    match &instr.value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            is_fusable_temp_lvalue(place)
        }
        _ => false,
    }
}

fn can_ignore_eval_order_fusion_temp_declaration(
    block: &[ReactiveStatement],
    tail_start: usize,
    participants: &[EvalOrderFusionParticipant<'_>],
    idx: usize,
) -> bool {
    let instr = participants[idx].instr;
    let decl_id = match &instr.value {
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => lvalue.place.identifier.declaration_id,
        _ => {
            return false;
        }
    };
    if reactive_instruction_writes_declaration(instr, decl_id)
        && reactive_instruction_uses_declaration(instr, decl_id)
    {
        // Ignore self-use checks for declaration instructions themselves.
    } else if reactive_instruction_uses_declaration(instr, decl_id) {
        return false;
    }
    for entry in &participants[idx + 1..] {
        if reactive_instruction_uses_declaration(entry.instr, decl_id) {
            return false;
        }
    }
    !reactive_block_uses_declaration(&block[tail_start..], decl_id)
}

fn is_side_effect_call_instruction(instr: &ReactiveInstruction) -> bool {
    instr.lvalue.is_none()
        && matches!(
            instr.value,
            InstructionValue::CallExpression { .. } | InstructionValue::MethodCall { .. }
        )
}

fn is_eval_order_fusion_participant(instr: &ReactiveInstruction) -> bool {
    if instr.lvalue.is_some() {
        is_fusable_inline_temp_instruction(instr)
    } else {
        is_side_effect_call_instruction(instr)
    }
}

fn wrap_sequence_expr(prefix_exprs: &[String], final_expr: String) -> String {
    if prefix_exprs.is_empty() {
        return final_expr;
    }
    let mut parts = Vec::with_capacity(prefix_exprs.len() + 1);
    parts.extend(prefix_exprs.iter().cloned());
    parts.push(final_expr);
    format!("({})", parts.join(", "))
}

fn render_side_effect_prefix_exprs_from_participants(
    cx: &mut Context,
    participants: &[EvalOrderFusionParticipant<'_>],
    indices: &[usize],
) -> Option<Vec<String>> {
    let mut exprs = Vec::with_capacity(indices.len());
    for idx in indices {
        let instr = participants.get(*idx)?.instr;
        if !is_side_effect_call_instruction(instr) {
            return None;
        }
        exprs.push(codegen_instruction_value_ev(cx, &instr.value).expr);
    }
    Some(exprs)
}

fn maybe_codegen_fused_method_call_destructure_assignment(
    cx: &mut Context,
    block: &[ReactiveStatement],
    start: usize,
    output: &mut String,
) -> Option<usize> {
    let ReactiveStatement::Instruction(first_instr) = block.get(start)? else {
        return None;
    };
    let first_lvalue = first_instr.lvalue.as_ref()?;
    if !is_fusable_temp_lvalue(first_lvalue) {
        return None;
    }
    if !matches!(
        first_instr.value,
        InstructionValue::LoadLocal { .. } | InstructionValue::LoadContext { .. }
    ) {
        return None;
    }

    let mut temp_instr_indices = vec![start];
    let mut destructure_pattern: Option<Pattern> = None;
    let mut destructure_value: Option<Place> = None;
    let mut destructure_idx: Option<usize> = None;
    let receiver_alias_decl = first_lvalue.identifier.declaration_id;
    let end_bound = (start + 8).min(block.len().saturating_sub(1));

    for idx in (start + 1)..=end_bound {
        let ReactiveStatement::Instruction(instr) = &block[idx] else {
            return None;
        };
        match &instr.value {
            InstructionValue::Destructure { lvalue, value, .. } => {
                if lvalue.kind != InstructionKind::Reassign || destructure_pattern.is_some() {
                    return None;
                }
                if pattern_operands(&lvalue.pattern)
                    .iter()
                    .any(|place| !cx.has_declared(&place.identifier))
                {
                    return None;
                }
                destructure_pattern = Some(lvalue.pattern.clone());
                destructure_value = Some(value.clone());
                destructure_idx = Some(idx);
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                receiver_optional,
                call_optional,
                ..
            } => {
                if instr.lvalue.is_some() {
                    return None;
                }
                if receiver.identifier.declaration_id != receiver_alias_decl {
                    return None;
                }
                let pattern = destructure_pattern.clone()?;
                let value_place = destructure_value.clone()?;
                if destructure_idx != Some(idx.saturating_sub(1)) {
                    return None;
                }

                let assign_arg_index = args.iter().position(|arg| {
                    matches!(
                        arg,
                        Argument::Place(place)
                            if place.identifier.declaration_id == value_place.identifier.declaration_id
                    )
                })?;

                let mut temp_decl_ids: HashSet<DeclarationId> = HashSet::new();
                for stmt_idx in &temp_instr_indices {
                    if let ReactiveStatement::Instruction(temp_instr) = &block[*stmt_idx]
                        && let Some(lvalue) = &temp_instr.lvalue
                    {
                        temp_decl_ids.insert(lvalue.identifier.declaration_id);
                    }
                }
                temp_decl_ids.insert(value_place.identifier.declaration_id);
                for decl_id in temp_decl_ids {
                    if reactive_block_uses_declaration(&block[idx + 1..], decl_id) {
                        return None;
                    }
                }

                for stmt_idx in &temp_instr_indices {
                    let ReactiveStatement::Instruction(temp_instr) = &block[*stmt_idx] else {
                        return None;
                    };
                    if !materialize_fusable_temp_instruction(cx, temp_instr) {
                        return None;
                    }
                }

                let pattern_expr = codegen_pattern(cx, &pattern);
                let rhs_expr =
                    codegen_place_with_min_prec(cx, &value_place, ExprPrecedence::Assignment);
                let assignment_expr = format!("({} = {})", pattern_expr, rhs_expr);

                let recv = codegen_member_object_expression(cx, receiver);
                let (prop, is_computed) = resolve_method_property(cx, property, &recv);
                let mut rendered_args: Vec<String> =
                    args.iter().map(|arg| codegen_argument(cx, arg)).collect();
                rendered_args[assign_arg_index] = assignment_expr;
                let hook_name = if is_computed {
                    None
                } else {
                    Some(prop.as_str())
                };
                maybe_replace_autodeps_with_inferred_deps(
                    cx,
                    hook_name.unwrap_or(""),
                    args,
                    &mut rendered_args,
                    hook_name.is_some(),
                );
                let args_str = join_call_arguments(&rendered_args);
                let call_expr = if is_computed {
                    let opt_recv = if *receiver_optional { "?." } else { "" };
                    if *call_optional {
                        format!("{}{}[{}]?.({})", recv, opt_recv, prop, args_str)
                    } else {
                        format!("{}{}[{}]({})", recv, opt_recv, prop, args_str)
                    }
                } else {
                    let dot = if *receiver_optional { "?." } else { "." };
                    if *call_optional {
                        format!("{}{}{}?.({})", recv, dot, prop, args_str)
                    } else if !*receiver_optional
                        && recv.starts_with("new ")
                        && (recv.contains('\n')
                            || prop == "build"
                            || recv.matches('.').count() >= 1)
                    {
                        format!("{}\n.{}({})", recv, prop, args_str)
                    } else {
                        format!("{}{}{}({})", recv, dot, prop, args_str)
                    }
                };
                let call_expr =
                    if cx.emit_hook_guards && hook_name.is_some_and(Environment::is_hook_name) {
                        wrap_hook_guarded_call_expression(&call_expr)
                    } else {
                        call_expr
                    };

                debug_codegen_expr(
                    "fused-method-call-destructure-arg",
                    format!(
                        "start={} end={} receiver={} property={} arg_index={}",
                        start, idx, recv, prop, assign_arg_index
                    ),
                );
                output.push_str(
                    &render_reactive_expression_statement_ast(&call_expr)
                        .unwrap_or_else(|| format!("{call_expr};\n")),
                );
                return Some(idx - start + 1);
            }
            _ => {
                if !is_fusable_inline_temp_instruction(instr) {
                    return None;
                }
                temp_instr_indices.push(idx);
            }
        }
    }

    None
}

fn materialize_fusable_temp_instruction(cx: &mut Context, instr: &ReactiveInstruction) -> bool {
    if !is_fusable_inline_temp_instruction(instr) {
        return false;
    }
    let Some(lvalue) = &instr.lvalue else {
        return false;
    };
    let ev = codegen_instruction_value_ev(cx, &instr.value);
    if is_temp_like_identifier(cx, &lvalue.identifier) {
        cx.inline_identifier_aliases
            .insert(lvalue.identifier.declaration_id, ev.expr.clone());
    }
    cx.set_temp_expr(&lvalue.identifier, Some(ev));
    true
}

fn maybe_defer_inlineable_ternary_into_following_scope(
    cx: &mut Context,
    instr: &ReactiveInstruction,
    preceding_stmts: &[ReactiveStatement],
    following_stmts: &[ReactiveStatement],
) -> bool {
    if cx.disable_memoization_features {
        return false;
    }
    let Some(lvalue) = &instr.lvalue else {
        return false;
    };
    if !is_fusable_temp_lvalue(lvalue) || !matches!(instr.value, InstructionValue::Ternary { .. }) {
        return false;
    }
    let definitions = collect_instruction_definitions(preceding_stmts);
    if !ternary_branches_include_jsx_value(instr, &definitions) {
        return false;
    }
    let Some(scope_id) = find_following_scope_for_deferred_inline_temp(
        following_stmts,
        lvalue.identifier.declaration_id,
    ) else {
        return false;
    };
    let mut root_deps = collect_root_reactive_dependencies_for_instruction(instr, preceding_stmts);
    if root_deps.is_empty() {
        return false;
    }
    root_deps.sort_by(compare_scope_dependency);
    if !materialize_fusable_temp_instruction(cx, instr) {
        return false;
    }
    cx.scope_dependency_overrides.insert(scope_id, root_deps);
    true
}

fn find_following_scope_for_deferred_inline_temp(
    following_stmts: &[ReactiveStatement],
    decl_id: DeclarationId,
) -> Option<ScopeId> {
    let mut target_scope_idx: Option<usize> = None;
    let mut target_scope_id: Option<ScopeId> = None;

    for (idx, stmt) in following_stmts.iter().enumerate().take(8) {
        match stmt {
            ReactiveStatement::Instruction(instr)
                if can_bridge_deferred_inline_temp_instruction(instr, decl_id) => {}
            ReactiveStatement::Scope(scope_block)
                if scope_block.scope.dependencies.len() == 1
                    && scope_block.scope.reassignments.is_empty()
                    && !scope_block.scope.declarations.is_empty()
                    && scope_declares_jsxish_value(scope_block)
                    && scope_block.scope.dependencies[0].identifier.declaration_id == decl_id =>
            {
                target_scope_idx = Some(idx);
                target_scope_id = Some(scope_block.scope.id);
                break;
            }
            _ => return None,
        }
    }

    let (scope_idx, scope_id) = target_scope_idx.zip(target_scope_id)?;
    if reactive_block_uses_declaration(&following_stmts[scope_idx + 1..], decl_id)
        || reactive_block_writes_declaration(&following_stmts[scope_idx + 1..], decl_id)
    {
        return None;
    }
    Some(scope_id)
}

fn collect_instruction_definitions(
    stmts: &[ReactiveStatement],
) -> HashMap<DeclarationId, &ReactiveInstruction> {
    let mut definitions = HashMap::new();
    for stmt in stmts {
        if let ReactiveStatement::Instruction(instr) = stmt
            && let Some(lvalue) = &instr.lvalue
        {
            definitions.insert(lvalue.identifier.declaration_id, &**instr);
        }
    }
    definitions
}

fn ternary_branches_include_jsx_value(
    instr: &ReactiveInstruction,
    definitions: &HashMap<DeclarationId, &ReactiveInstruction>,
) -> bool {
    let InstructionValue::Ternary {
        consequent,
        alternate,
        ..
    } = &instr.value
    else {
        return false;
    };

    let mut seen_defs = HashSet::new();
    place_resolves_to_jsxish_value(consequent, definitions, &mut seen_defs)
        || place_resolves_to_jsxish_value(alternate, definitions, &mut seen_defs)
}

fn scope_declares_jsxish_value(scope_block: &ReactiveScopeBlock) -> bool {
    let definitions = collect_instruction_definitions(&scope_block.instructions);
    scope_block.scope.declarations.values().any(|decl| {
        let place = Place {
            identifier: decl.identifier.clone(),
            effect: Effect::Read,
            reactive: true,
            loc: SourceLocation::Generated,
        };
        let mut seen_defs = HashSet::new();
        place_resolves_to_jsxish_value(&place, &definitions, &mut seen_defs)
    })
}

fn place_resolves_to_jsxish_value(
    place: &Place,
    definitions: &HashMap<DeclarationId, &ReactiveInstruction>,
    seen_defs: &mut HashSet<DeclarationId>,
) -> bool {
    let Some(def_instr) = definitions.get(&place.identifier.declaration_id).copied() else {
        return false;
    };
    if !seen_defs.insert(place.identifier.declaration_id) {
        return false;
    }
    let result = match &def_instr.value {
        InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. } => true,
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. }
        | InstructionValue::TypeCastExpression { value: place, .. } => {
            place_resolves_to_jsxish_value(place, definitions, seen_defs)
        }
        InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } => {
            place_resolves_to_jsxish_value(consequent, definitions, seen_defs)
                || place_resolves_to_jsxish_value(alternate, definitions, seen_defs)
        }
        _ => false,
    };
    seen_defs.remove(&place.identifier.declaration_id);
    result
}

fn can_bridge_deferred_inline_temp_instruction(
    instr: &ReactiveInstruction,
    decl_id: DeclarationId,
) -> bool {
    let Some(lvalue) = &instr.lvalue else {
        return false;
    };
    lvalue.identifier.name.is_none()
        && is_fusable_inline_temp_instruction(instr)
        && !reactive_instruction_uses_declaration(instr, decl_id)
        && !reactive_instruction_writes_declaration(instr, decl_id)
}

fn collect_root_reactive_dependencies_for_instruction(
    instr: &ReactiveInstruction,
    preceding_stmts: &[ReactiveStatement],
) -> Vec<ReactiveScopeDependency> {
    let definitions = collect_instruction_definitions(preceding_stmts);

    let mut deps = Vec::new();
    let mut seen_defs = HashSet::new();
    visitors::for_each_instruction_value_operand(&instr.value, |place| {
        collect_root_reactive_dependencies_for_place(
            place,
            &definitions,
            &mut seen_defs,
            &mut deps,
        );
    });
    deps
}

fn collect_root_reactive_dependencies_for_place(
    place: &Place,
    definitions: &HashMap<DeclarationId, &ReactiveInstruction>,
    seen_defs: &mut HashSet<DeclarationId>,
    deps: &mut Vec<ReactiveScopeDependency>,
) {
    if place.reactive && place.identifier.name.is_some() {
        push_unique_scope_dependency(
            deps,
            ReactiveScopeDependency {
                identifier: place.identifier.clone(),
                path: Vec::new(),
            },
        );
        return;
    }

    let Some(def_instr) = definitions.get(&place.identifier.declaration_id).copied() else {
        if place.reactive {
            push_unique_scope_dependency(
                deps,
                ReactiveScopeDependency {
                    identifier: place.identifier.clone(),
                    path: Vec::new(),
                },
            );
        }
        return;
    };

    if !seen_defs.insert(place.identifier.declaration_id) {
        return;
    }
    visitors::for_each_instruction_value_operand(&def_instr.value, |inner| {
        collect_root_reactive_dependencies_for_place(inner, definitions, seen_defs, deps);
    });
    seen_defs.remove(&place.identifier.declaration_id);
}

fn push_unique_scope_dependency(
    deps: &mut Vec<ReactiveScopeDependency>,
    candidate: ReactiveScopeDependency,
) {
    if deps.iter().any(|existing| {
        existing.identifier.declaration_id == candidate.identifier.declaration_id
            && existing.path.len() == candidate.path.len()
            && existing
                .path
                .iter()
                .zip(&candidate.path)
                .all(|(left, right)| {
                    left.property == right.property && left.optional == right.optional
                })
    }) {
        return;
    }
    deps.push(candidate);
}

fn is_fusable_inline_temp_instruction(instr: &ReactiveInstruction) -> bool {
    let Some(lvalue) = &instr.lvalue else {
        return false;
    };
    if !is_fusable_temp_lvalue(lvalue) {
        return false;
    }
    !matches!(
        instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::ObjectMethod { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::MethodCall { .. }
    )
}

fn is_fusable_temp_lvalue(place: &Place) -> bool {
    match place.identifier.name.as_ref() {
        None => true,
        Some(IdentifierName::Named(name)) | Some(IdentifierName::Promoted(name)) => {
            is_codegen_temp_name(name.as_str())
        }
    }
}

fn reactive_block_uses_declaration(
    block: &[ReactiveStatement],
    declaration_id: DeclarationId,
) -> bool {
    for stmt in block {
        if reactive_statement_uses_declaration(stmt, declaration_id) {
            return true;
        }
    }
    false
}

fn reactive_block_writes_declaration(
    block: &[ReactiveStatement],
    declaration_id: DeclarationId,
) -> bool {
    for stmt in block {
        if reactive_statement_writes_declaration(stmt, declaration_id) {
            return true;
        }
    }
    false
}

fn reactive_statement_uses_declaration(
    stmt: &ReactiveStatement,
    declaration_id: DeclarationId,
) -> bool {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            reactive_instruction_uses_declaration(instr, declaration_id)
        }
        ReactiveStatement::Terminal(term_stmt) => {
            reactive_terminal_uses_declaration(&term_stmt.terminal, declaration_id)
        }
        ReactiveStatement::Scope(scope_block) => {
            reactive_block_uses_declaration(&scope_block.instructions, declaration_id)
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            reactive_block_uses_declaration(&scope_block.instructions, declaration_id)
        }
    }
}

fn reactive_statement_writes_declaration(
    stmt: &ReactiveStatement,
    declaration_id: DeclarationId,
) -> bool {
    match stmt {
        ReactiveStatement::Instruction(instr) => {
            reactive_instruction_writes_declaration(instr, declaration_id)
        }
        ReactiveStatement::Terminal(term_stmt) => {
            reactive_terminal_writes_declaration(&term_stmt.terminal, declaration_id)
        }
        ReactiveStatement::Scope(scope_block) => {
            reactive_block_writes_declaration(&scope_block.instructions, declaration_id)
        }
        ReactiveStatement::PrunedScope(scope_block) => {
            reactive_block_writes_declaration(&scope_block.instructions, declaration_id)
        }
    }
}

fn reactive_instruction_uses_declaration(
    instr: &ReactiveInstruction,
    declaration_id: DeclarationId,
) -> bool {
    if instr
        .lvalue
        .as_ref()
        .is_some_and(|place| place.identifier.declaration_id == declaration_id)
    {
        return true;
    }

    let mut has_operand = false;
    visitors::for_each_instruction_value_operand(&instr.value, |place| {
        if place.identifier.declaration_id == declaration_id {
            has_operand = true;
        }
    });
    if has_operand {
        return true;
    }

    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            lvalue.place.identifier.declaration_id == declaration_id
        }
        InstructionValue::Destructure { lvalue, .. } => pattern_operands(&lvalue.pattern)
            .iter()
            .any(|place| place.identifier.declaration_id == declaration_id),
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            lvalue.identifier.declaration_id == declaration_id
        }
        _ => false,
    }
}

fn reactive_instruction_writes_declaration(
    instr: &ReactiveInstruction,
    declaration_id: DeclarationId,
) -> bool {
    if instr
        .lvalue
        .as_ref()
        .is_some_and(|place| place.identifier.declaration_id == declaration_id)
    {
        return true;
    }

    match &instr.value {
        InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. }
        | InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            lvalue.place.identifier.declaration_id == declaration_id
        }
        InstructionValue::Destructure { lvalue, .. } => pattern_operands(&lvalue.pattern)
            .iter()
            .any(|place| place.identifier.declaration_id == declaration_id),
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => {
            lvalue.identifier.declaration_id == declaration_id
        }
        _ => false,
    }
}

fn reactive_terminal_uses_declaration(
    terminal: &ReactiveTerminal,
    declaration_id: DeclarationId,
) -> bool {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => false,
        ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
            value.identifier.declaration_id == declaration_id
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            test.identifier.declaration_id == declaration_id
                || reactive_block_uses_declaration(consequent, declaration_id)
                || alternate
                    .as_ref()
                    .is_some_and(|block| reactive_block_uses_declaration(block, declaration_id))
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            test.identifier.declaration_id == declaration_id
                || cases.iter().any(|case| {
                    case.test
                        .as_ref()
                        .is_some_and(|place| place.identifier.declaration_id == declaration_id)
                        || case.block.as_ref().is_some_and(|block| {
                            reactive_block_uses_declaration(block, declaration_id)
                        })
                })
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            reactive_block_uses_declaration(init, declaration_id)
                || test.identifier.declaration_id == declaration_id
                || update
                    .as_ref()
                    .is_some_and(|block| reactive_block_uses_declaration(block, declaration_id))
                || reactive_block_uses_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            reactive_block_uses_declaration(init, declaration_id)
                || test.identifier.declaration_id == declaration_id
                || reactive_block_uses_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            reactive_block_uses_declaration(init, declaration_id)
                || reactive_block_uses_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        }
        | ReactiveTerminal::DoWhile {
            test, loop_block, ..
        } => {
            test.identifier.declaration_id == declaration_id
                || reactive_block_uses_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::Label { block, .. } => {
            reactive_block_uses_declaration(block, declaration_id)
        }
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            handler_binding
                .as_ref()
                .is_some_and(|place| place.identifier.declaration_id == declaration_id)
                || reactive_block_uses_declaration(block, declaration_id)
                || reactive_block_uses_declaration(handler, declaration_id)
        }
    }
}

fn reactive_terminal_writes_declaration(
    terminal: &ReactiveTerminal,
    declaration_id: DeclarationId,
) -> bool {
    match terminal {
        ReactiveTerminal::Break { .. } | ReactiveTerminal::Continue { .. } => false,
        ReactiveTerminal::Return { .. } | ReactiveTerminal::Throw { .. } => false,
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            reactive_block_writes_declaration(consequent, declaration_id)
                || alternate
                    .as_ref()
                    .is_some_and(|block| reactive_block_writes_declaration(block, declaration_id))
        }
        ReactiveTerminal::Switch { cases, .. } => cases.iter().any(|case| {
            case.block
                .as_ref()
                .is_some_and(|block| reactive_block_writes_declaration(block, declaration_id))
        }),
        ReactiveTerminal::DoWhile {
            test, loop_block, ..
        } => {
            let _ = test;
            reactive_block_writes_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            let _ = test;
            reactive_block_writes_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            ..
        } => {
            let _ = test;
            reactive_block_writes_declaration(init, declaration_id)
                || update
                    .as_ref()
                    .is_some_and(|block| reactive_block_writes_declaration(block, declaration_id))
                || reactive_block_writes_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            let _ = test;
            reactive_block_writes_declaration(init, declaration_id)
                || reactive_block_writes_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            reactive_block_writes_declaration(init, declaration_id)
                || reactive_block_writes_declaration(loop_block, declaration_id)
        }
        ReactiveTerminal::Label { block, .. } => {
            reactive_block_writes_declaration(block, declaration_id)
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            reactive_block_writes_declaration(block, declaration_id)
                || reactive_block_writes_declaration(handler, declaration_id)
        }
    }
}

fn maybe_codegen_inline_literal_init_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    if !scope_block.scope.dependencies.is_empty()
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.instructions.len() != 1
    {
        return None;
    }
    let decl = scope_block.scope.declarations.values().next()?;
    let ReactiveStatement::Instruction(source_instr) = scope_block.instructions.first()? else {
        return None;
    };
    let source_lvalue = source_instr.lvalue.as_ref()?;
    if source_lvalue.identifier.declaration_id != decl.identifier.declaration_id {
        return None;
    }
    if !matches!(
        source_instr.value,
        InstructionValue::Primitive { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
    ) {
        return None;
    }

    let ReactiveStatement::Instruction(store_instr) = following_stmts.first()? else {
        return None;
    };
    let (store_lvalue, store_value) = match &store_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => (lvalue, value),
        _ => return None,
    };
    if store_value.identifier.declaration_id != decl.identifier.declaration_id
        || store_lvalue.place.identifier.name.is_none()
    {
        return None;
    }
    let Some(parent_scope) = &store_lvalue.place.identifier.scope else {
        return None;
    };
    if parent_scope.dependencies.is_empty() {
        return None;
    }

    let rhs = codegen_instruction_value_ev(cx, &source_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);
    let kind = if has_materialized_named_binding(cx, &store_lvalue.place.identifier) {
        InstructionKind::Reassign
    } else {
        store_lvalue.kind
    };
    if let Some(stmt) = codegen_store(cx, store_instr, kind, &store_lvalue.place, &rhs) {
        output.push_str(&stmt);
        if !stmt.ends_with('\n') {
            output.push('\n');
        }
    }
    Some(1)
}

fn codegen_scope_computation_no_reset(
    cx: &mut Context,
    scope: &ReactiveScope,
    block: &ReactiveBlock,
) -> String {
    if should_drop_trailing_scope_temp_declare(scope, block) {
        return codegen_block_no_reset_with_options(cx, &block[..block.len() - 1], true);
    }

    if let Some((target_ident, rhs_place)) = scope_tail_sequence_reassign(scope, block) {
        let mut output = String::new();
        for stmt in &block[..block.len().saturating_sub(1)] {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if let Some(stmt_text) = codegen_instruction_nullable(cx, instr) {
                        output.push_str(&stmt_text);
                        if !stmt_text.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                }
                ReactiveStatement::PrunedScope(pruned) => {
                    output.push_str(&codegen_block_no_reset(cx, &pruned.instructions));
                }
                ReactiveStatement::Scope(scope_block) => {
                    let temp_snapshot = cx.snapshot_temps();
                    codegen_reactive_scope(
                        cx,
                        &mut output,
                        &scope_block.scope,
                        &scope_block.instructions,
                    );
                    cx.restore_temps(temp_snapshot);
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    if let Some(stmt_text) = codegen_terminal(cx, &term_stmt.terminal) {
                        output.push_str(&stmt_text);
                        if !stmt_text.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                }
            }
        }
        let target_name = identifier_name_with_cx(cx, target_ident);
        let rhs_expr = codegen_place_to_expression(cx, rhs_place);
        if rhs_expr == "undefined"
            && matches!(rhs_place.identifier.loc, SourceLocation::Generated)
            && !output.contains("break bb")
        {
            return output;
        }
        output.push_str(
            &render_reactive_assignment_statement_ast(&target_name, &rhs_expr)
                .unwrap_or_else(|| format!("{} = {};\n", target_name, rhs_expr)),
        );
        return output;
    }

    codegen_block_no_reset_with_options(cx, block, true)
}

fn should_drop_trailing_scope_temp_declare(scope: &ReactiveScope, block: &ReactiveBlock) -> bool {
    if block.is_empty()
        || !scope.dependencies.is_empty()
        || !scope.declarations.is_empty()
        || scope.reassignments.len() != 1
    {
        return false;
    }
    let ReactiveStatement::Instruction(last_instr) = &block[block.len() - 1] else {
        return false;
    };
    let decl_lvalue = match &last_instr.value {
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => lvalue,
        _ => return false,
    };
    if !is_fusable_temp_lvalue(&decl_lvalue.place) {
        return false;
    }
    let decl_id = decl_lvalue.place.identifier.declaration_id;
    if reactive_block_uses_declaration(&block[..block.len() - 1], decl_id) {
        return false;
    }
    debug_codegen_expr(
        "scope-drop-trailing-temp-declare",
        format!("scope={} decl_id={}", scope.id.0, decl_id.0),
    );
    true
}

fn scope_tail_sequence_reassign<'a>(
    scope: &'a ReactiveScope,
    block: &'a ReactiveBlock,
) -> Option<(&'a Identifier, &'a Place)> {
    if scope.reassignments.len() != 1 || block.is_empty() {
        return None;
    }
    let target_decl = scope.reassignments[0].declaration_id;
    let ReactiveStatement::Instruction(last_instr) = block.last()? else {
        return None;
    };
    match &last_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. }
            if lvalue.kind == InstructionKind::Reassign
                && lvalue.place.identifier.declaration_id == target_decl =>
        {
            debug_codegen_expr(
                "sequence-tail-reassign",
                format!(
                    "scope={} target_decl={} source_decl={}",
                    scope.id.0,
                    lvalue.place.identifier.declaration_id.0,
                    value.identifier.declaration_id.0
                ),
            );
            Some((&lvalue.place.identifier, value))
        }
        _ => None,
    }
}

fn maybe_codegen_fused_ternary_source_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let debug = std::env::var("DEBUG_SCOPE_INLINE").is_ok();
    if scope_block.scope.dependencies.len() != 1
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
    {
        if debug {
            eprintln!(
                "[SCOPE_FUSE_TERNARY] reject precheck deps={} reassignments={} decls={}",
                scope_block.scope.dependencies.len(),
                scope_block.scope.reassignments.len(),
                scope_block.scope.declarations.len()
            );
        }
        return None;
    }
    let decl = scope_block.scope.declarations.values().next()?;
    if debug {
        let mut descs = Vec::new();
        for stmt in following_stmts.iter().take(3) {
            let desc = match stmt {
                ReactiveStatement::Instruction(instr) => {
                    let has_lv = instr.lvalue.is_some();
                    let kind = match &instr.value {
                        InstructionValue::Ternary { .. } => "Ternary",
                        InstructionValue::StoreLocal { .. } => "StoreLocal",
                        InstructionValue::StoreContext { .. } => "StoreContext",
                        InstructionValue::LoadLocal { .. } => "LoadLocal",
                        InstructionValue::LoadContext { .. } => "LoadContext",
                        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
                        InstructionValue::DeclareContext { .. } => "DeclareContext",
                        _ => "Other",
                    };
                    format!("Instruction(kind={}, lvalue={})", kind, has_lv)
                }
                ReactiveStatement::Scope(_) => "Scope".to_string(),
                ReactiveStatement::PrunedScope(_) => "PrunedScope".to_string(),
                ReactiveStatement::Terminal(_) => "Terminal".to_string(),
            };
            descs.push(desc);
        }
        eprintln!("[SCOPE_FUSE_TERNARY] lookahead={:?}", descs);
    }
    let mut ternary_idx: Option<usize> = None;
    let mut scope_is_consequent: Option<bool> = None;
    for (idx, stmt) in following_stmts.iter().enumerate().take(8) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        if idx > 0
            && !is_ignorable_bridge_interstitial(instr)
            && !matches!(&instr.value, InstructionValue::Ternary { .. })
        {
            continue;
        }
        let Some(lvalue) = &instr.lvalue else {
            continue;
        };
        let InstructionValue::Ternary {
            consequent,
            alternate,
            ..
        } = &instr.value
        else {
            continue;
        };
        if debug {
            eprintln!(
                "[SCOPE_FUSE_TERNARY] inspect ternary idx={} cons_decl={} scope_decl={} lv_decl={} alt={}",
                idx,
                consequent.identifier.declaration_id.0,
                decl.identifier.declaration_id.0,
                lvalue.identifier.declaration_id.0,
                codegen_place_to_expression(cx, alternate)
            );
        }
        let cons_is_scope = consequent.identifier.declaration_id == decl.identifier.declaration_id;
        let alt_is_scope = alternate.identifier.declaration_id == decl.identifier.declaration_id;
        if !cons_is_scope && !alt_is_scope {
            continue;
        }
        if lvalue.identifier.declaration_id == decl.identifier.declaration_id {
            continue;
        }
        ternary_idx = Some(idx);
        scope_is_consequent = Some(cons_is_scope);
        break;
    }
    let Some(ternary_idx) = ternary_idx else {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: no matching ternary lookahead");
        }
        return None;
    };
    let scope_is_consequent = scope_is_consequent?;
    for stmt in &following_stmts[..ternary_idx] {
        let ReactiveStatement::Instruction(instr) = stmt else {
            if debug {
                eprintln!("[SCOPE_FUSE_TERNARY] reject: non-instruction before ternary");
            }
            return None;
        };
        if !is_ignorable_bridge_interstitial(instr) {
            if debug {
                eprintln!("[SCOPE_FUSE_TERNARY] reject: non-ignorable instruction before ternary");
            }
            return None;
        }
        let _ = codegen_instruction_nullable(cx, instr);
    }
    let Some(ReactiveStatement::Instruction(ternary_instr)) = following_stmts.get(ternary_idx)
    else {
        return None;
    };
    let Some(ternary_lvalue) = &ternary_instr.lvalue else {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: matching ternary has no lvalue");
        }
        return None;
    };
    let InstructionValue::Ternary {
        test,
        consequent,
        alternate,
        ..
    } = &ternary_instr.value
    else {
        return None;
    };
    let non_scope_branch_expr_raw = if scope_is_consequent {
        codegen_place_to_expression(cx, alternate)
    } else {
        codegen_place_to_expression(cx, consequent)
    };
    let non_scope_is_inlineable_primitive =
        is_inlineable_primitive_literal_expression(non_scope_branch_expr_raw.trim());
    if non_scope_branch_expr_raw != "null" && !non_scope_is_inlineable_primitive {
        if debug {
            eprintln!(
                "[SCOPE_FUSE_TERNARY] reject: ternary non-scope branch resolves to {}",
                non_scope_branch_expr_raw
            );
        }
        return None;
    }
    if scope_block.instructions.is_empty() {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: scope has no instructions");
        }
        return None;
    }
    let source_idx = scope_block.instructions.len() - 1;
    for prefix_stmt in &scope_block.instructions[..source_idx] {
        let ReactiveStatement::Instruction(prefix_instr) = prefix_stmt else {
            if debug {
                eprintln!("[SCOPE_FUSE_TERNARY] reject: non-instruction prefix in scope");
            }
            return None;
        };
        let emitted = codegen_instruction_nullable(cx, prefix_instr);
        if emitted
            .as_deref()
            .is_some_and(|stmt| !stmt.trim().is_empty())
        {
            if debug {
                eprintln!(
                    "[SCOPE_FUSE_TERNARY] reject: prefix emits statement `{}`",
                    emitted.as_deref().unwrap_or_default()
                );
            }
            return None;
        }
    }
    let Some(ReactiveStatement::Instruction(source_instr)) =
        scope_block.instructions.get(source_idx)
    else {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: scope source stmt is not instruction");
        }
        return None;
    };
    let Some(source_lvalue) = &source_instr.lvalue else {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: source instruction has no lvalue");
        }
        return None;
    };
    if source_lvalue.identifier.declaration_id != decl.identifier.declaration_id {
        if debug {
            eprintln!(
                "[SCOPE_FUSE_TERNARY] reject: source decl {} != scope decl {}",
                source_lvalue.identifier.declaration_id.0, decl.identifier.declaration_id.0
            );
        }
        return None;
    }
    if matches!(
        &source_instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::ObjectMethod { .. }
    ) {
        if debug {
            eprintln!("[SCOPE_FUSE_TERNARY] reject: source instruction kind not supported");
        }
        return None;
    }

    let raw_dep = &scope_block.scope.dependencies[0];
    if debug {
        let path_desc: Vec<String> = raw_dep
            .path
            .iter()
            .map(|entry| {
                format!(
                    "{}{}",
                    if entry.optional { "?." } else { "." },
                    entry.property
                )
            })
            .collect();
        eprintln!(
            "[SCOPE_FUSE_TERNARY] raw_dep id={} name={:?} path={:?}",
            raw_dep.identifier.id.0, raw_dep.identifier.name, path_desc
        );
    }
    let cond_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
    let dep_for_codegen = truncate_ref_current_dep(raw_dep, &cx.stable_ref_decls);
    let mut dep_expr = codegen_dependency(cx, &dep_for_codegen);
    let mut force_root_dep = false;
    if !scope_is_consequent && !dep_for_codegen.path.is_empty() {
        let root_expr = identifier_name_with_cx(cx, &dep_for_codegen.identifier);
        if cond_expr.contains(&format!("{}?.", root_expr)) {
            dep_expr = root_expr;
            force_root_dep = true;
        }
    }
    let cond_is_non_local = cx
        .non_local_binding_decls
        .contains(&test.identifier.declaration_id);
    let include_cond_dep = !force_root_dep
        && !cond_is_non_local
        && !is_inlineable_primitive_literal_expression(cond_expr.trim());
    let source_ev = codegen_instruction_value_ev(cx, &source_instr.value);
    let mut source_expr = source_ev.wrap_if_needed(ExprPrecedence::Conditional);
    let should_compact_source_expr = match &source_instr.value {
        InstructionValue::ObjectExpression { properties, .. } => {
            properties.iter().any(|property| {
                matches!(
                    property,
                    ObjectPropertyOrSpread::Property(ObjectProperty {
                        type_: ObjectPropertyType::Method,
                        ..
                    })
                )
            })
        }
        InstructionValue::CallExpression { .. } => true,
        _ => false,
    };
    if source_expr.contains('\n') && should_compact_source_expr && !source_expr.contains('`') {
        source_expr = compact_single_statement(&source_expr);
    }
    let consequent_expr = if scope_is_consequent {
        source_expr.clone()
    } else {
        codegen_place_with_min_prec(cx, consequent, ExprPrecedence::Conditional)
    };
    let alternate_expr = if scope_is_consequent {
        codegen_place_with_min_prec(cx, alternate, ExprPrecedence::Conditional)
    } else {
        source_expr.clone()
    };
    if debug {
        eprintln!(
            "[SCOPE_FUSE_TERNARY] expr cond=`{}` dep=`{}` source=`{}` cons=`{}` alt=`{}`",
            cond_expr, dep_expr, source_expr, consequent_expr, alternate_expr
        );
    }

    let cache_var = cx.synthesize_name("$");
    let cond_slot = include_cond_dep.then(|| cx.alloc_cache_slot());
    let dep_slot = cx.alloc_cache_slot();
    let output_slot = cx.alloc_cache_slot();
    let decl_name = identifier_name_with_cx(cx, &decl.identifier);
    if !has_materialized_named_binding(cx, &decl.identifier) {
        output.push_str(
            &render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Let,
                &decl_name,
                None,
            )
            .unwrap_or_else(|| format!("let {};\n", decl_name)),
        );
        cx.mark_decl_runtime_emitted(decl.identifier.declaration_id);
    }
    cx.declare(&decl.identifier);

    if debug {
        eprintln!(
            "[SCOPE_FUSE_TERNARY] cond_dep include={} non_local={} decl={}",
            include_cond_dep, cond_is_non_local, test.identifier.declaration_id.0
        );
    }

    let guard_test = if let Some(cond_slot) = cond_slot {
        format!(
            "{}[{}] !== {} || {}[{}] !== {}",
            cache_var, cond_slot, cond_expr, cache_var, dep_slot, dep_expr
        )
    } else {
        format!("{}[{}] !== {}", cache_var, dep_slot, dep_expr)
    };
    let mut consequent = render_reactive_assignment_statement_ast(
        &decl_name,
        &format!("{} ? {} : {}", cond_expr, consequent_expr, alternate_expr),
    )
    .unwrap_or_else(|| {
        format!(
            "{} = {} ? {} : {};\n",
            decl_name, cond_expr, consequent_expr, alternate_expr
        )
    });
    if let Some(cond_slot) = cond_slot {
        consequent.push_str(
            &render_reactive_expression_statement_ast(&format!(
                "{}[{}] = {}",
                cache_var, cond_slot, cond_expr
            ))
            .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, cond_slot, cond_expr)),
        );
    }
    consequent.push_str(
        &render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, dep_slot, dep_expr
        ))
        .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, dep_slot, dep_expr)),
    );
    consequent.push_str(
        &render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, output_slot, decl_name
        ))
        .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, output_slot, decl_name)),
    );
    let alternate = render_reactive_assignment_statement_ast(
        &decl_name,
        &format!("{}[{}]", cache_var, output_slot),
    )
    .unwrap_or_else(|| format!("{} = {}[{}];\n", decl_name, cache_var, output_slot));
    output.push_str(
        &render_reactive_if_statement_ast(&guard_test, &consequent, Some(&alternate))
            .unwrap_or_else(|| {
                format!(
                    "if ({}) {{\n{}}} else {{\n{}}}\n",
                    guard_test, consequent, alternate
                )
            }),
    );

    // The skipped ternary instruction's lvalue aliases the fused scope output.
    cx.set_temp_expr(
        &ternary_lvalue.identifier,
        Some(ExprValue::primary(decl_name)),
    );
    let suppressed_display_idx = ternary_lvalue
        .identifier
        .name
        .as_ref()
        .and_then(|name| match name {
            IdentifierName::Named(n) | IdentifierName::Promoted(n) => n
                .strip_prefix('t')
                .and_then(|suffix| suffix.parse::<u32>().ok()),
        })
        .unwrap_or(ternary_lvalue.identifier.id.0);
    if !cx.suppressed_temp_ids.contains(&suppressed_display_idx) {
        cx.suppressed_temp_ids.push(suppressed_display_idx);
    }
    if debug {
        eprintln!("[SCOPE_FUSE_TERNARY] accept");
    }
    Some(ternary_idx + 1)
}

fn maybe_codegen_inline_zero_dep_literal_into_following_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let debug = std::env::var("DEBUG_SCOPE_INLINE").is_ok();
    let (source_decl, source_instr) = zero_dep_single_decl_scope_source(scope_block)?;
    if !matches!(
        &source_instr.value,
        InstructionValue::Primitive { .. }
            | InstructionValue::ArrayExpression { .. }
            | InstructionValue::ObjectExpression { .. }
    ) {
        return None;
    }
    let source_lvalue = source_instr.lvalue.as_ref()?;

    let ReactiveStatement::Scope(next_scope) = following_stmts.first()? else {
        return None;
    };
    if next_scope.scope.dependencies.is_empty() {
        return None;
    }
    let ReactiveStatement::Instruction(first_instr) = next_scope.instructions.first()? else {
        return None;
    };
    let (store_lvalue, store_value) = match &first_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => (lvalue, value),
        _ => return None,
    };
    if store_value.identifier.declaration_id != source_decl
        || store_lvalue.place.identifier.name.is_none()
        || first_instr.lvalue.is_some()
    {
        return None;
    }
    if reactive_block_uses_declaration(&next_scope.instructions[1..], source_decl) {
        return None;
    }

    let target_ident = &store_lvalue.place.identifier;
    let source_seed_expr = codegen_instruction_value_ev(cx, &source_instr.value);

    // Alias the seed temp to the literal itself while generating the
    // following dep scope, so the initial store is emitted inside the
    // recompute branch as `target = literal`.
    let temp_snapshot = cx.snapshot_temps();
    cx.set_temp_expr(&source_lvalue.identifier, Some(source_seed_expr));
    codegen_reactive_scope(cx, output, &next_scope.scope, &next_scope.instructions);
    cx.restore_temps(temp_snapshot);

    let suppressed_display_idx = source_lvalue
        .identifier
        .name
        .as_ref()
        .and_then(|name| match name {
            IdentifierName::Named(n) | IdentifierName::Promoted(n) => n
                .strip_prefix('t')
                .and_then(|suffix| suffix.parse::<u32>().ok()),
        })
        .unwrap_or(source_lvalue.identifier.id.0);
    if !cx.suppressed_temp_ids.contains(&suppressed_display_idx) {
        cx.suppressed_temp_ids.push(suppressed_display_idx);
    }

    if debug {
        eprintln!(
            "[SCOPE_INLINE_ZERO_DEP] accept source_decl={} target_decl={} scope={}",
            source_decl.0, target_ident.declaration_id.0, next_scope.scope.id.0
        );
    }

    Some(1)
}

fn maybe_codegen_fused_dual_zero_dep_literal_ternary_scope(
    cx: &mut Context,
    first_scope: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let (first_decl, first_source_instr) = zero_dep_single_decl_scope_source(first_scope)?;

    let ReactiveStatement::Instruction(first_store_instr) = following_stmts.first()? else {
        return None;
    };
    let (target_ident, first_store_result_decl) =
        parse_named_store_from_decl(first_store_instr, first_decl)?;
    let first_store_result_decl = first_store_result_decl?;

    let ReactiveStatement::Scope(second_scope) = following_stmts.get(1)? else {
        return None;
    };
    let (second_decl, second_source_instr) = zero_dep_single_decl_scope_source(second_scope)?;

    let ReactiveStatement::Instruction(second_store_instr) = following_stmts.get(2)? else {
        return None;
    };
    let (second_target_ident, second_store_result_decl) =
        parse_named_store_from_decl(second_store_instr, second_decl)?;
    if second_target_ident.declaration_id != target_ident.declaration_id {
        return None;
    }
    let second_store_result_decl = second_store_result_decl?;

    let ReactiveStatement::Instruction(ternary_instr) = following_stmts.get(3)? else {
        return None;
    };
    if ternary_instr.lvalue.is_some() {
        return None;
    }
    let InstructionValue::Ternary {
        test,
        consequent,
        alternate,
        ..
    } = &ternary_instr.value
    else {
        return None;
    };

    let (cons_source_instr, alt_source_instr) = if consequent.identifier.declaration_id
        == first_store_result_decl
        && alternate.identifier.declaration_id == second_store_result_decl
    {
        (first_source_instr, second_source_instr)
    } else if consequent.identifier.declaration_id == second_store_result_decl
        && alternate.identifier.declaration_id == first_store_result_decl
    {
        (second_source_instr, first_source_instr)
    } else {
        return None;
    };

    if reactive_block_uses_declaration(&following_stmts[4..], first_store_result_decl)
        || reactive_block_uses_declaration(&following_stmts[4..], second_store_result_decl)
    {
        return None;
    }

    let target_name = identifier_name_with_cx(cx, target_ident);
    let cond_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
    let cons_expr = codegen_instruction_value_ev(cx, &cons_source_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);
    let alt_expr = codegen_instruction_value_ev(cx, &alt_source_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);
    let computation = format!(
        "{} ? ({} = {}) : ({} = {});\n",
        cond_expr, target_name, cons_expr, target_name, alt_expr
    );
    emit_zero_dep_target_guard(cx, output, target_ident, &computation);
    debug_codegen_expr(
        "fused-dual-zero-dep-ternary-scope",
        format!(
            "target={} first_decl={} second_decl={}",
            target_name, first_decl.0, second_decl.0
        ),
    );
    Some(4)
}

fn maybe_codegen_fused_zero_dep_literal_store_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let (scope_decl, source_instr) = zero_dep_single_decl_scope_source(scope_block)?;

    let ReactiveStatement::Instruction(store_instr) = following_stmts.first()? else {
        return None;
    };
    let (target_ident, store_result_decl) = parse_named_store_from_decl(store_instr, scope_decl)?;
    if cx
        .function_decl_decls
        .contains(&target_ident.declaration_id)
    {
        return None;
    }
    if reactive_block_writes_declaration(&following_stmts[1..], target_ident.declaration_id) {
        return None;
    }
    if matches!(following_stmts.get(1), Some(ReactiveStatement::Scope(_))) {
        return None;
    }

    let target_name = identifier_name_with_cx(cx, target_ident);
    let source_expr = codegen_instruction_value_ev(cx, &source_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);

    let mut logical_idx: Option<usize> = None;
    let mut bridge_instrs: Vec<&ReactiveInstruction> = Vec::new();
    let mut cursor = 1usize;
    while let Some(stmt) = following_stmts.get(cursor) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        if instr.lvalue.is_none()
            && matches!(instr.value, InstructionValue::LogicalExpression { .. })
        {
            logical_idx = Some(cursor);
            break;
        }
        if is_fusable_inline_temp_instruction(instr) {
            bridge_instrs.push(instr);
            cursor += 1;
            continue;
        }
        break;
    }

    let mut consumed_following = 1usize;
    let computation = if let Some(logical_idx) = logical_idx {
        for instr in bridge_instrs {
            if !materialize_fusable_temp_instruction(cx, instr) {
                return None;
            }
        }
        let ReactiveStatement::Instruction(logical_instr) = &following_stmts[logical_idx] else {
            return None;
        };
        let InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } = &logical_instr.value
        else {
            return None;
        };
        let logical_prec = logical_operator_precedence(operator);
        let left_expr = codegen_logical_operand(cx, left, logical_prec);
        let logical_op = logical_operator_to_str(operator);
        let right_is_store_result =
            store_result_decl.is_some_and(|decl| right.identifier.declaration_id == decl);
        let right_expr = if right_is_store_result {
            format!("({} = {})", target_name, source_expr)
        } else {
            let right_expr_raw = codegen_place_to_expression(cx, right);
            if *operator == LogicalOperator::And && right_expr_raw == "null" {
                format!("(({} = {}), null)", target_name, source_expr)
            } else {
                return None;
            }
        };
        consumed_following = logical_idx + 1;
        format!("{} {} {};\n", left_expr, logical_op, right_expr)
    } else {
        if let Some(store_result_decl) = store_result_decl
            && reactive_block_uses_declaration(&following_stmts[1..], store_result_decl)
        {
            return None;
        }
        format!("{} = {};\n", target_name, source_expr)
    };

    if !has_direct_return_tail_for_decl(
        &following_stmts[consumed_following..],
        target_ident.declaration_id,
    ) {
        return None;
    }

    emit_zero_dep_target_guard(cx, output, target_ident, &computation);
    debug_codegen_expr(
        "fused-zero-dep-literal-store-scope",
        format!(
            "target={} consumed_following={}",
            target_name, consumed_following
        ),
    );
    Some(consumed_following)
}

fn maybe_codegen_fused_zero_dep_sequence_logical_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let (target_ident, source_instr, scope_decl) =
        parse_zero_dep_sequence_null_scope_target(scope_block)?;
    if cx
        .function_decl_decls
        .contains(&target_ident.declaration_id)
    {
        return None;
    }
    let ReactiveStatement::Instruction(logical_instr) = following_stmts.first()? else {
        return None;
    };
    let InstructionValue::LogicalExpression {
        operator,
        left,
        right,
        ..
    } = &logical_instr.value
    else {
        return None;
    };
    if *operator != LogicalOperator::And || right.identifier.declaration_id != scope_decl {
        return None;
    }
    if reactive_block_uses_declaration(&following_stmts[1..], scope_decl)
        || reactive_block_writes_declaration(&following_stmts[1..], target_ident.declaration_id)
    {
        return None;
    }
    if !has_direct_return_tail_for_decl(&following_stmts[1..], target_ident.declaration_id) {
        return None;
    }

    let logical_prec = logical_operator_precedence(operator);
    let left_expr = codegen_logical_operand(cx, left, logical_prec);
    let target_name = identifier_name_with_cx(cx, target_ident);
    let source_expr = codegen_instruction_value_ev(cx, &source_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);
    let computation = format!(
        "{} && (({} = {}), null);\n",
        left_expr, target_name, source_expr
    );
    emit_zero_dep_target_guard(cx, output, target_ident, &computation);
    debug_codegen_expr(
        "fused-zero-dep-sequence-logical-scope",
        format!("target={} scope_decl={}", target_name, scope_decl.0),
    );
    Some(1)
}

fn parse_zero_dep_sequence_null_scope_target(
    scope_block: &ReactiveScopeBlock,
) -> Option<(&Identifier, &ReactiveInstruction, DeclarationId)> {
    if !scope_block.scope.dependencies.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.scope.reassignments.len() != 1
        || scope_block.instructions.len() != 4
    {
        return None;
    }
    let target_ident = &scope_block.scope.reassignments[0];
    let scope_decl = scope_block
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .declaration_id;

    let ReactiveStatement::Instruction(source_instr) = &scope_block.instructions[0] else {
        return None;
    };
    let source_decl = source_instr
        .lvalue
        .as_ref()
        .map(|lvalue| lvalue.identifier.declaration_id)?;

    let ReactiveStatement::Instruction(store_instr) = &scope_block.instructions[1] else {
        return None;
    };
    let (store_lvalue, store_value) = match &store_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => (lvalue, value),
        _ => return None,
    };
    if store_lvalue.kind != InstructionKind::Reassign
        || store_lvalue.place.identifier.declaration_id != target_ident.declaration_id
        || store_value.identifier.declaration_id != source_decl
    {
        return None;
    }

    let ReactiveStatement::Instruction(null_instr) = &scope_block.instructions[2] else {
        return None;
    };
    let null_decl = match &null_instr.value {
        InstructionValue::Primitive {
            value: PrimitiveValue::Null,
            ..
        } => null_instr
            .lvalue
            .as_ref()
            .map(|lvalue| lvalue.identifier.declaration_id)?,
        _ => return None,
    };

    let ReactiveStatement::Instruction(scope_decl_instr) = &scope_block.instructions[3] else {
        return None;
    };
    let scope_decl_lvalue = scope_decl_instr
        .lvalue
        .as_ref()
        .map(|lvalue| lvalue.identifier.declaration_id)?;
    let InstructionValue::LoadLocal { place, .. } = &scope_decl_instr.value else {
        return None;
    };
    if scope_decl_lvalue != scope_decl || place.identifier.declaration_id != null_decl {
        return None;
    }

    Some((target_ident, source_instr, scope_decl))
}

fn has_direct_return_tail_for_decl(
    trailing_stmts: &[ReactiveStatement],
    target_decl: DeclarationId,
) -> bool {
    match trailing_stmts {
        [ReactiveStatement::Terminal(term_stmt)] => matches!(
            &term_stmt.terminal,
            ReactiveTerminal::Return { value, .. }
                if value.identifier.declaration_id == target_decl
        ),
        [
            ReactiveStatement::Instruction(load_instr),
            ReactiveStatement::Terminal(term_stmt),
        ] => {
            let InstructionValue::LoadLocal { place, .. } = &load_instr.value else {
                return false;
            };
            if place.identifier.declaration_id != target_decl {
                return false;
            }
            let Some(load_result_decl) = load_instr
                .lvalue
                .as_ref()
                .map(|lvalue| lvalue.identifier.declaration_id)
            else {
                return false;
            };
            matches!(
                &term_stmt.terminal,
                ReactiveTerminal::Return { value, .. }
                    if value.identifier.declaration_id == load_result_decl
            )
        }
        _ => false,
    }
}

fn zero_dep_single_decl_scope_source(
    scope_block: &ReactiveScopeBlock,
) -> Option<(DeclarationId, &ReactiveInstruction)> {
    if !scope_block.scope.dependencies.is_empty()
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.instructions.len() != 1
    {
        return None;
    }
    let decl = scope_block.scope.declarations.values().next()?;
    let ReactiveStatement::Instruction(source_instr) = scope_block.instructions.first()? else {
        return None;
    };
    let source_lvalue = source_instr.lvalue.as_ref()?;
    if source_lvalue.identifier.declaration_id != decl.identifier.declaration_id {
        return None;
    }
    if matches!(
        &source_instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::FunctionExpression { .. }
            | InstructionValue::ObjectMethod { .. }
    ) {
        return None;
    }
    Some((decl.identifier.declaration_id, source_instr))
}

fn parse_named_store_from_decl(
    store_instr: &ReactiveInstruction,
    source_decl: DeclarationId,
) -> Option<(&Identifier, Option<DeclarationId>)> {
    let (store_lvalue, store_value) = match &store_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => (lvalue, value),
        _ => return None,
    };
    if store_value.identifier.declaration_id != source_decl
        || store_lvalue.place.identifier.name.is_none()
    {
        return None;
    }
    let store_result_decl = store_instr
        .lvalue
        .as_ref()
        .map(|lvalue| lvalue.identifier.declaration_id);
    Some((&store_lvalue.place.identifier, store_result_decl))
}

fn emit_zero_dep_target_guard(
    cx: &mut Context,
    output: &mut String,
    target_ident: &Identifier,
    computation_stmt: &str,
) {
    let cache_var = cx.synthesize_name("$");
    let output_slot = cx.alloc_cache_slot();
    let target_name = identifier_name_with_cx(cx, target_ident);
    if !has_materialized_named_binding(cx, target_ident) {
        output.push_str(
            &render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Let,
                &target_name,
                None,
            )
            .unwrap_or_else(|| format!("let {};\n", target_name)),
        );
        cx.mark_decl_runtime_emitted(target_ident.declaration_id);
    }
    cx.declare(target_ident);
    let mut consequent = computation_stmt.to_string();
    consequent.push_str(
        &render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, output_slot, target_name
        ))
        .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, output_slot, target_name)),
    );
    let alternate = render_reactive_assignment_statement_ast(
        &target_name,
        &format!("{}[{}]", cache_var, output_slot),
    )
    .unwrap_or_else(|| format!("{} = {}[{}];\n", target_name, cache_var, output_slot));
    let guard_test = format!(
        "{}[{}] === Symbol.for(\"{}\")",
        cache_var, output_slot, MEMO_CACHE_SENTINEL
    );
    output.push_str(
        &render_reactive_if_statement_ast(&guard_test, &consequent, Some(&alternate))
            .unwrap_or_else(|| {
                format!(
                    "if ({}) {{\n{}}} else {{\n{}}}\n",
                    guard_test, consequent, alternate
                )
            }),
    );
}

fn maybe_codegen_fused_zero_dep_ternary_default_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    let debug = std::env::var("DEBUG_SCOPE_INLINE").is_ok();
    if !scope_block.scope.dependencies.is_empty()
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.instructions.is_empty()
    {
        if debug {
            eprintln!(
                "[SCOPE_FUSE_ZERO_DEP_DEFAULT] reject precheck deps={} reassignments={} decls={} instrs={}",
                scope_block.scope.dependencies.len(),
                scope_block.scope.reassignments.len(),
                scope_block.scope.declarations.len(),
                scope_block.instructions.len()
            );
        }
        return None;
    }

    let decl = scope_block.scope.declarations.values().next()?;
    let source_decl_id = decl.identifier.declaration_id;
    let mut probe_cx = cx.clone();
    let mut source_instr: Option<&ReactiveInstruction> = None;
    for stmt in &scope_block.instructions {
        let ReactiveStatement::Instruction(instr) = stmt else {
            return None;
        };
        let source_expr = codegen_instruction_value_ev(&mut probe_cx, &instr.value);
        let Some(lvalue) = &instr.lvalue else {
            return None;
        };
        probe_cx
            .temp
            .insert(lvalue.identifier.declaration_id, Some(source_expr));
        if lvalue.identifier.declaration_id == source_decl_id {
            source_instr = Some(instr);
        }
    }
    let source_instr = source_instr?;
    let source_lvalue = source_instr.lvalue.as_ref()?;
    if source_lvalue.identifier.declaration_id != source_decl_id {
        return None;
    }
    if matches!(
        &source_instr.value,
        InstructionValue::StoreLocal { .. }
            | InstructionValue::StoreContext { .. }
            | InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::Destructure { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::Debugger { .. }
            | InstructionValue::ObjectMethod { .. }
    ) {
        if debug {
            eprintln!("[SCOPE_FUSE_ZERO_DEP_DEFAULT] reject: unsupported source instruction");
        }
        return None;
    }

    let mut source_alias_decls: HashSet<DeclarationId> = HashSet::new();
    source_alias_decls.insert(source_decl_id);

    let mut ternary_idx: Option<usize> = None;
    let mut source_is_consequent = false;
    let mut source_place: Option<Place> = None;
    let mut dep_place: Option<Place> = None;
    for (idx, stmt) in following_stmts.iter().enumerate().take(8) {
        let ReactiveStatement::Instruction(instr) = stmt else {
            break;
        };
        if let Some(lvalue) = &instr.lvalue {
            let source_derived = match &instr.value {
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::LoadContext { place, .. } => {
                    source_alias_decls.contains(&place.identifier.declaration_id)
                }
                InstructionValue::TypeCastExpression { value, .. } => {
                    source_alias_decls.contains(&value.identifier.declaration_id)
                }
                _ => false,
            };
            if source_derived {
                source_alias_decls.insert(lvalue.identifier.declaration_id);
            }
        }
        if idx > 0
            && !is_ignorable_bridge_interstitial(instr)
            && !matches!(&instr.value, InstructionValue::Ternary { .. })
        {
            continue;
        }
        let Some(_) = &instr.lvalue else {
            continue;
        };
        let InstructionValue::Ternary {
            test: _,
            consequent,
            alternate,
            ..
        } = &instr.value
        else {
            continue;
        };
        let cons_is_source = source_alias_decls.contains(&consequent.identifier.declaration_id);
        let alt_is_source = source_alias_decls.contains(&alternate.identifier.declaration_id);
        if cons_is_source == alt_is_source {
            continue;
        }
        let candidate_dep = if cons_is_source {
            alternate
        } else {
            consequent
        };
        if candidate_dep.identifier.declaration_id == decl.identifier.declaration_id {
            continue;
        }
        let dep_expr = codegen_place_to_expression(cx, candidate_dep);
        if !is_valid_js_identifier_name(dep_expr.trim()) {
            if debug {
                eprintln!(
                    "[SCOPE_FUSE_ZERO_DEP_DEFAULT] reject: non-simple dep expr `{}`",
                    dep_expr
                );
            }
            continue;
        }
        ternary_idx = Some(idx);
        source_is_consequent = cons_is_source;
        source_place = Some(if cons_is_source {
            consequent.clone()
        } else {
            alternate.clone()
        });
        dep_place = Some(candidate_dep.clone());
        break;
    }

    let ternary_idx = ternary_idx?;
    let source_seed_expr = probe_cx
        .temp
        .get(&source_decl_id)
        .cloned()
        .flatten()
        .unwrap_or_else(|| codegen_instruction_value_ev(&mut probe_cx, &source_instr.value));
    let cx_snapshot = cx.clone();
    cx.set_temp_expr(&source_lvalue.identifier, Some(source_seed_expr));
    for stmt in &following_stmts[..ternary_idx] {
        let ReactiveStatement::Instruction(instr) = stmt else {
            *cx = cx_snapshot;
            return None;
        };
        let bridge_stmt = codegen_instruction_nullable(cx, instr);
        if bridge_stmt
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            if debug {
                eprintln!(
                    "[SCOPE_FUSE_ZERO_DEP_DEFAULT] reject: bridge emits statement `{}`",
                    bridge_stmt.as_deref().unwrap_or_default()
                );
            }
            *cx = cx_snapshot;
            return None;
        }
    }

    let Some(ReactiveStatement::Instruction(ternary_instr)) = following_stmts.get(ternary_idx)
    else {
        return None;
    };
    let Some(ternary_lvalue) = &ternary_instr.lvalue else {
        return None;
    };
    let InstructionValue::Ternary {
        test,
        consequent: _,
        alternate: _,
        ..
    } = &ternary_instr.value
    else {
        *cx = cx_snapshot;
        return None;
    };
    let dep_place = dep_place?;
    let source_place = source_place?;

    let cond_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
    let source_expr = codegen_place_with_min_prec(cx, &source_place, ExprPrecedence::Conditional);
    let dep_expr_cond = codegen_place_with_min_prec(cx, &dep_place, ExprPrecedence::Conditional);
    let dep_expr_guard = codegen_place_to_expression(cx, &dep_place);

    let use_ternary_lvalue =
        ternary_lvalue
            .identifier
            .name
            .as_ref()
            .is_some_and(|name| match name {
                IdentifierName::Named(n) | IdentifierName::Promoted(n) => {
                    !(n.starts_with('t') && n[1..].chars().all(|ch| ch.is_ascii_digit()))
                }
            });
    let output_ident = if use_ternary_lvalue {
        &ternary_lvalue.identifier
    } else {
        &decl.identifier
    };
    let output_name = identifier_name_with_cx(cx, output_ident);
    let cache_var = cx.synthesize_name("$");
    let dep_slot = cx.alloc_cache_slot();
    let output_slot = cx.alloc_cache_slot();

    if !has_materialized_named_binding(cx, output_ident) {
        output.push_str(
            &render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Let,
                &output_name,
                None,
            )
            .unwrap_or_else(|| format!("let {};\n", output_name)),
        );
        cx.mark_decl_runtime_emitted(output_ident.declaration_id);
    }
    cx.declare(output_ident);

    let rhs_expr = if source_is_consequent {
        format!("{} ? {} : {}", cond_expr, source_expr, dep_expr_cond)
    } else {
        format!("{} ? {} : {}", cond_expr, dep_expr_cond, source_expr)
    };

    let mut consequent = render_reactive_assignment_statement_ast(&output_name, &rhs_expr)
        .unwrap_or_else(|| format!("{} = {};\n", output_name, rhs_expr));
    consequent.push_str(
        &render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, dep_slot, dep_expr_guard
        ))
        .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, dep_slot, dep_expr_guard)),
    );
    consequent.push_str(
        &render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, output_slot, output_name
        ))
        .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, output_slot, output_name)),
    );
    let alternate = render_reactive_assignment_statement_ast(
        &output_name,
        &format!("{}[{}]", cache_var, output_slot),
    )
    .unwrap_or_else(|| format!("{} = {}[{}];\n", output_name, cache_var, output_slot));
    let guard_test = format!("{}[{}] !== {}", cache_var, dep_slot, dep_expr_guard);
    output.push_str(
        &render_reactive_if_statement_ast(&guard_test, &consequent, Some(&alternate))
            .unwrap_or_else(|| {
                format!(
                    "if ({}) {{\n{}}} else {{\n{}}}\n",
                    guard_test, consequent, alternate
                )
            }),
    );

    cx.set_temp_expr(
        &ternary_lvalue.identifier,
        Some(ExprValue::primary(output_name.clone())),
    );
    cx.temp.insert(
        decl.identifier.declaration_id,
        Some(ExprValue::primary(output_name)),
    );
    let suppressed_display_idx = ternary_lvalue
        .identifier
        .name
        .as_ref()
        .and_then(|name| match name {
            IdentifierName::Named(n) | IdentifierName::Promoted(n) => n
                .strip_prefix('t')
                .and_then(|suffix| suffix.parse::<u32>().ok()),
        })
        .unwrap_or(ternary_lvalue.identifier.id.0);
    if !cx.suppressed_temp_ids.contains(&suppressed_display_idx) {
        cx.suppressed_temp_ids.push(suppressed_display_idx);
    }
    if debug {
        eprintln!("[SCOPE_FUSE_ZERO_DEP_DEFAULT] accept");
    }

    Some(ternary_idx + 1)
}

fn maybe_codegen_fused_callback_reassign_scope(
    cx: &mut Context,
    callback_scope: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    // Pattern (narrow):
    //   1) zero-dep callback temp scope:
    //      scope { tX = () => ... }
    //   2) optional ignorable load/primitive bridge instructions
    //   3) dep scope with non-empty deps (typically mutating callback capture source)
    //   4) load+store alias into named var:
    //      tY = tX; named = tY;
    //
    // Emit a single guard keyed by dep scope deps and cache the named callback.
    if !callback_scope.scope.dependencies.is_empty()
        || !callback_scope.scope.reassignments.is_empty()
        || callback_scope.scope.declarations.len() != 1
        || callback_scope.instructions.len() != 1
    {
        return None;
    }

    let callback_decl = callback_scope
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .declaration_id;

    let ReactiveStatement::Instruction(callback_instr) = callback_scope.instructions.first()?
    else {
        return None;
    };
    let callback_lvalue = callback_instr.lvalue.as_ref()?;
    if callback_lvalue.identifier.declaration_id != callback_decl {
        return None;
    }
    if !matches!(
        callback_instr.value,
        InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
    ) {
        return None;
    }

    // Find the following dependency scope, allowing only ignorable bridge instructions.
    let mut dep_scope_idx: Option<usize> = None;
    for (idx, stmt) in following_stmts.iter().enumerate() {
        match stmt {
            ReactiveStatement::Instruction(instr) if is_ignorable_bridge_interstitial(instr) => {}
            ReactiveStatement::Scope(scope_block) => {
                if scope_block.scope.dependencies.is_empty() {
                    return None;
                }
                dep_scope_idx = Some(idx);
                break;
            }
            _ => return None,
        }
    }
    let dep_scope_idx = dep_scope_idx?;
    let ReactiveStatement::Scope(dep_scope) = following_stmts.get(dep_scope_idx)? else {
        return None;
    };

    // Find load+store alias into a named callback variable after the dep scope.
    // Allow a small postfix sequence of interstitial instructions (e.g. ternary bridge)
    // before the alias pair.
    let mut post_dep_instructions: Vec<&ReactiveInstruction> = Vec::new();
    let mut cursor = dep_scope_idx + 1;
    let (target_ident, consumed_after_dep) = loop {
        let ReactiveStatement::Instruction(instr) = following_stmts.get(cursor)? else {
            return None;
        };
        let load_lvalue = instr.lvalue.as_ref();
        let load_from_callback = match &instr.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                place.identifier.declaration_id == callback_decl
            }
            _ => false,
        };
        if load_from_callback {
            let alias_load_lvalue = load_lvalue?;
            let alias_temp_decl = alias_load_lvalue.identifier.declaration_id;
            let ReactiveStatement::Instruction(alias_store) = following_stmts.get(cursor + 1)?
            else {
                return None;
            };
            let (target_ident, alias_matches) = match &alias_store.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => (
                    lvalue.place.identifier.clone(),
                    value.identifier.declaration_id == alias_temp_decl
                        && lvalue.place.identifier.name.is_some(),
                ),
                _ => return None,
            };
            if !alias_matches {
                return None;
            }
            break (target_ident, (cursor + 2) - dep_scope_idx);
        }
        let is_reassign_store = matches!(
            instr.value,
            InstructionValue::StoreLocal { ref lvalue, .. }
                | InstructionValue::StoreContext { ref lvalue, .. }
                if lvalue.kind == InstructionKind::Reassign
        );
        if is_ignorable_bridge_interstitial(instr)
            || matches!(instr.value, InstructionValue::Ternary { .. })
            || is_reassign_store
        {
            post_dep_instructions.push(instr);
            cursor += 1;
            continue;
        }
        return None;
    };

    // Truncate ref.current deps to just ref (upstream: PropagateScopeDependenciesHIR.ts L610-620).
    let truncated_deps: Vec<ReactiveScopeDependency> = dep_scope
        .scope
        .dependencies
        .iter()
        .map(|dep| truncate_ref_current_dep(dep, &cx.stable_ref_decls))
        .collect();
    let mut sorted_deps: Vec<&ReactiveScopeDependency> = truncated_deps.iter().collect();
    sort_scope_dependency_refs_for_codegen(cx, &mut sorted_deps);
    let mut seen_dep_keys: HashSet<String> = HashSet::new();
    let mut dep_exprs: Vec<String> = Vec::new();
    let mut post_dep_reassign_decls: HashSet<DeclarationId> = HashSet::new();
    for instr in &post_dep_instructions {
        if let InstructionValue::StoreLocal { lvalue, .. }
        | InstructionValue::StoreContext { lvalue, .. } = &instr.value
            && lvalue.kind == InstructionKind::Reassign
        {
            post_dep_reassign_decls.insert(lvalue.place.identifier.declaration_id);
        }
    }
    for dep in sorted_deps {
        let key = format_dependency_name(dep);
        if seen_dep_keys.insert(key) {
            dep_exprs.push(codegen_dependency(cx, dep));
        }
    }
    // Include additional deps observed in pre-dep bridge loads (e.g. `foo` in
    // callback+control-flow patterns).
    for stmt in &following_stmts[..dep_scope_idx] {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        if let InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } = &instr.value
            && place.identifier.name.is_some()
            && !post_dep_reassign_decls.contains(&place.identifier.declaration_id)
        {
            let expr = codegen_place_to_expression(cx, place);
            if !is_inlineable_primitive_literal_expression(expr.trim())
                && seen_dep_keys.insert(expr.clone())
            {
                dep_exprs.push(expr);
            }
        }
    }
    // Include additional deps observed in post-dep bridge loads (e.g. ternary test `foo`).
    for instr in &post_dep_instructions {
        if let InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } = &instr.value
            && place.identifier.name.is_some()
            && !post_dep_reassign_decls.contains(&place.identifier.declaration_id)
        {
            let expr = codegen_place_to_expression(cx, place);
            if !is_inlineable_primitive_literal_expression(expr.trim())
                && seen_dep_keys.insert(expr.clone())
            {
                dep_exprs.push(expr);
            }
        }
    }
    dep_exprs.sort();
    dep_exprs.dedup();
    if dep_exprs.is_empty() {
        return None;
    }

    let target_name = identifier_name_with_cx(cx, &target_ident);
    if target_name.is_empty() {
        return None;
    }

    if !has_materialized_named_binding(cx, &target_ident) {
        output.push_str(&render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            &target_name,
            None,
        )?);
        cx.mark_decl_runtime_emitted(target_ident.declaration_id);
    }
    cx.declare(&target_ident);

    let temp_snapshot = cx.snapshot_temps();

    let cache_var = cx.synthesize_name("$");
    let mut change_exprs: Vec<String> = Vec::new();
    let mut cache_store_stmts: Vec<String> = Vec::new();
    for dep_expr in &dep_exprs {
        let dep_slot = cx.alloc_cache_slot();
        change_exprs.push(format!("{}[{}] !== {}", cache_var, dep_slot, dep_expr));
        cache_store_stmts.push(render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, dep_slot, dep_expr
        ))?);
    }
    let callback_slot = cx.alloc_cache_slot();

    let callback_ev = codegen_instruction_value_ev(cx, &callback_instr.value);
    let callback_expr = callback_ev.wrap_if_needed(ExprPrecedence::Assignment);

    let mut computation = String::new();
    computation.push_str(&format!("{} = {};\n", target_name, callback_expr));

    for stmt in &following_stmts[..dep_scope_idx] {
        let ReactiveStatement::Instruction(instr) = stmt else {
            cx.restore_temps(temp_snapshot);
            return None;
        };
        if let Some(stmt_text) = codegen_instruction_nullable(cx, instr) {
            computation.push_str(&stmt_text);
            if !stmt_text.ends_with('\n') {
                computation.push('\n');
            }
        }
    }

    computation.push_str(&codegen_scope_computation_no_reset(
        cx,
        &dep_scope.scope,
        &dep_scope.instructions,
    ));
    for instr in post_dep_instructions {
        if let Some(stmt_text) = codegen_instruction_nullable(cx, instr) {
            computation.push_str(&stmt_text);
            if !stmt_text.ends_with('\n') {
                computation.push('\n');
            }
        }
    }
    cache_store_stmts.push(render_reactive_expression_statement_ast(&format!(
        "{}[{}] = {}",
        cache_var, callback_slot, target_name
    ))?);

    let mut consequent = computation;
    for stmt in &cache_store_stmts {
        consequent.push_str(stmt);
    }
    output.push_str(&render_reactive_if_statement_ast(
        &change_exprs.join(" || "),
        &consequent,
        Some(&render_reactive_assignment_statement_ast(
            &target_name,
            &format!("{}[{}]", cache_var, callback_slot),
        )?),
    )?);

    cx.restore_temps(temp_snapshot);
    Some(dep_scope_idx + consumed_after_dep)
}

fn is_zero_dep_empty_array_scope(stmt: &ReactiveStatement) -> Option<DeclarationId> {
    let ReactiveStatement::Scope(scope_block) = stmt else {
        return None;
    };
    let (decl_id, source_instr) = zero_dep_single_decl_scope_source(scope_block)?;
    match &source_instr.value {
        InstructionValue::ArrayExpression { elements, .. } if elements.is_empty() => Some(decl_id),
        _ => None,
    }
}

fn args_match_effect_callback_and_deps(
    args: &[Argument],
    callback_decl: DeclarationId,
    deps_decl: DeclarationId,
) -> bool {
    let Some(callback_idx) = args.iter().position(|arg| {
        matches!(arg, Argument::Place(place) if place.identifier.declaration_id == callback_decl)
    }) else {
        return false;
    };
    args.iter().skip(callback_idx + 1).any(
        |arg| matches!(arg, Argument::Place(place) if place.identifier.declaration_id == deps_decl),
    )
}

fn maybe_codegen_fused_effect_callback_empty_array_scope(
    cx: &mut Context,
    callback_scope: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
    output: &mut String,
) -> Option<usize> {
    if !callback_scope.scope.reassignments.is_empty()
        || callback_scope.scope.declarations.len() != 1
        || callback_scope.instructions.len() != 1
    {
        return None;
    }

    let callback_decl = callback_scope
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .declaration_id;
    let callback_ident = callback_scope
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .clone();
    let ReactiveStatement::Instruction(callback_instr) = callback_scope.instructions.first()?
    else {
        return None;
    };
    let callback_lvalue = callback_instr.lvalue.as_ref()?;
    if callback_lvalue.identifier.declaration_id != callback_decl {
        return None;
    }
    if !matches!(
        callback_instr.value,
        InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
    ) {
        return None;
    }

    let filtered_callback_deps: Vec<ReactiveScopeDependency> = callback_scope
        .scope
        .dependencies
        .iter()
        .filter(|dep| {
            !cx.stable_zero_dep_decls
                .contains(&dep.identifier.declaration_id)
        })
        .cloned()
        .collect();
    if !filtered_callback_deps.is_empty() {
        return None;
    }

    let ReactiveStatement::Scope(deps_scope) = following_stmts.first()? else {
        return None;
    };
    if !deps_scope.scope.dependencies.is_empty()
        || !deps_scope.scope.reassignments.is_empty()
        || deps_scope.scope.declarations.len() != 1
        || deps_scope.instructions.len() != 1
    {
        return None;
    }

    let deps_decl = deps_scope
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .declaration_id;
    let deps_ident = deps_scope
        .scope
        .declarations
        .values()
        .next()?
        .identifier
        .clone();
    let ReactiveStatement::Instruction(deps_instr) = deps_scope.instructions.first()? else {
        return None;
    };
    let deps_lvalue = deps_instr.lvalue.as_ref()?;
    if deps_lvalue.identifier.declaration_id != deps_decl {
        return None;
    }
    if !matches!(
        &deps_instr.value,
        InstructionValue::ArrayExpression { elements, .. } if elements.is_empty()
    ) {
        return None;
    }

    let ReactiveStatement::Instruction(hook_instr) = following_stmts.get(1)? else {
        return None;
    };
    let matches_effect_hook = match &hook_instr.value {
        InstructionValue::CallExpression { callee, args, .. } => resolve_place_name(cx, callee)
            .and_then(|name| extract_hook_name(&name).map(str::to_string))
            .is_some_and(|hook_name| {
                is_effect_like_hook_name(&hook_name)
                    && args_match_effect_callback_and_deps(args, callback_decl, deps_decl)
            }),
        InstructionValue::MethodCall { property, args, .. } => resolve_place_name(cx, property)
            .and_then(|name| extract_hook_name(&name).map(str::to_string))
            .is_some_and(|hook_name| {
                is_effect_like_hook_name(&hook_name)
                    && args_match_effect_callback_and_deps(args, callback_decl, deps_decl)
            }),
        _ => false,
    };
    if !matches_effect_hook {
        return None;
    }

    let callback_name = identifier_name_with_cx(cx, &callback_ident);
    let deps_name = identifier_name_with_cx(cx, &deps_ident);
    if callback_name.is_empty() || deps_name.is_empty() {
        return None;
    }

    let cache_var = cx.synthesize_name("$");
    let callback_slot = cx.alloc_cache_slot();
    let deps_slot = cx.alloc_cache_slot();
    let callback_expr = codegen_instruction_value_ev(cx, &callback_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);
    let deps_expr = codegen_instruction_value_ev(cx, &deps_instr.value)
        .wrap_if_needed(ExprPrecedence::Assignment);

    let mut rendered = String::new();
    if !has_materialized_named_binding(cx, &callback_ident) {
        rendered.push_str(&render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            &callback_name,
            None,
        )?);
        cx.mark_decl_runtime_emitted(callback_ident.declaration_id);
    }
    if !has_materialized_named_binding(cx, &deps_ident) {
        rendered.push_str(&render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            &deps_name,
            None,
        )?);
        cx.mark_decl_runtime_emitted(deps_ident.declaration_id);
    }
    cx.declare(&callback_ident);
    cx.declare(&deps_ident);

    let consequent = format!(
        "{}{}{}{}",
        render_reactive_assignment_statement_ast(&callback_name, &callback_expr)?,
        render_reactive_assignment_statement_ast(&deps_name, &deps_expr)?,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, callback_slot, callback_name
        ))?,
        render_reactive_expression_statement_ast(&format!(
            "{}[{}] = {}",
            cache_var, deps_slot, deps_name
        ))?,
    );
    let alternate = format!(
        "{}{}",
        render_reactive_assignment_statement_ast(
            &callback_name,
            &format!("{}[{}]", cache_var, callback_slot),
        )?,
        render_reactive_assignment_statement_ast(
            &deps_name,
            &format!("{}[{}]", cache_var, deps_slot),
        )?,
    );
    rendered.push_str(&render_reactive_if_statement_ast(
        &format!(
            "{}[{}] === Symbol.for(\"{}\")",
            cache_var, callback_slot, MEMO_CACHE_SENTINEL
        ),
        &consequent,
        Some(&alternate),
    )?);
    output.push_str(&rendered);

    cx.stable_zero_dep_decls
        .insert(callback_ident.declaration_id);
    cx.stable_zero_dep_decls.insert(deps_ident.declaration_id);

    Some(1)
}

fn filtered_effect_callback_deps_for_explicit_empty_array(
    cx: &Context,
    scope_block: &ReactiveScopeBlock,
    following_stmts: &[ReactiveStatement],
) -> Option<Vec<ReactiveScopeDependency>> {
    if scope_block.scope.dependencies.is_empty()
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.instructions.len() != 1
    {
        return None;
    }

    let callback_decl = scope_block
        .scope
        .declarations
        .values()
        .next()
        .map(|decl| decl.identifier.declaration_id)?;
    let ReactiveStatement::Instruction(callback_instr) = scope_block.instructions.first()? else {
        return None;
    };
    if callback_instr
        .lvalue
        .as_ref()
        .is_none_or(|lvalue| lvalue.identifier.declaration_id != callback_decl)
    {
        return None;
    }
    if !matches!(
        callback_instr.value,
        InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
    ) {
        return None;
    }

    let deps_decl = following_stmts
        .first()
        .and_then(is_zero_dep_empty_array_scope)?;
    let Some(ReactiveStatement::Instruction(hook_instr)) = following_stmts.get(1) else {
        return None;
    };

    let matches_effect_hook = match &hook_instr.value {
        InstructionValue::CallExpression { callee, args, .. } => resolve_place_name(cx, callee)
            .and_then(|name| extract_hook_name(&name).map(str::to_string))
            .is_some_and(|hook_name| {
                is_effect_like_hook_name(&hook_name)
                    && args_match_effect_callback_and_deps(args, callback_decl, deps_decl)
            }),
        InstructionValue::MethodCall { property, args, .. } => resolve_place_name(cx, property)
            .and_then(|name| extract_hook_name(&name).map(str::to_string))
            .is_some_and(|hook_name| {
                is_effect_like_hook_name(&hook_name)
                    && args_match_effect_callback_and_deps(args, callback_decl, deps_decl)
            }),
        _ => false,
    };

    if !matches_effect_hook {
        return None;
    }

    let filtered: Vec<ReactiveScopeDependency> = scope_block
        .scope
        .dependencies
        .iter()
        .filter(|dep| {
            !cx.stable_zero_dep_decls
                .contains(&dep.identifier.declaration_id)
        })
        .cloned()
        .collect();

    if filtered.len() == scope_block.scope.dependencies.len() {
        return None;
    }

    if std::env::var("DEBUG_SCOPE_DEP_OVERRIDE").is_ok() {
        eprintln!(
            "[SCOPE_DEP_OVERRIDE] drop stable zero-dep effect callback deps callback_decl={} scope={} before={:?} after={:?}",
            callback_decl.0,
            scope_block.scope.id.0,
            scope_block
                .scope
                .dependencies
                .iter()
                .map(format_dependency_name)
                .collect::<Vec<_>>(),
            filtered
                .iter()
                .map(format_dependency_name)
                .collect::<Vec<_>>()
        );
    }

    Some(filtered)
}

fn should_inline_zero_dep_global_zero_arg_call_scope(
    cx: &mut Context,
    scope_block: &ReactiveScopeBlock,
    preceding_stmts: &[ReactiveStatement],
    following_stmts: &[ReactiveStatement],
) -> bool {
    if !scope_block.scope.dependencies.is_empty()
        || !scope_block.scope.reassignments.is_empty()
        || scope_block.scope.declarations.len() != 1
        || scope_block.instructions.len() != 1
    {
        return false;
    }
    // Keep this memo scope when downstream calls still consume AUTODEPS values
    // (typical hook lowering), otherwise we can lose an upstream cache slot.
    let has_autodeps_call =
        block_contains_autodeps_call_usage(following_stmts, &mut HashSet::new());
    let has_hook_call = block_contains_hook_call(cx, following_stmts);
    if has_autodeps_call {
        return false;
    }
    // Keep this memo scope when downstream statements contain hook calls.
    // Upstream frequently preserves a cache slot for values captured by later hooks.
    if has_hook_call {
        return false;
    }
    // Avoid inlining inside blocks that already passed through control-flow
    // terminals (if/switch/loops/try), where upstream usually preserves memo slots.
    if preceding_stmts
        .iter()
        .any(|stmt| matches!(stmt, ReactiveStatement::Terminal(_)))
    {
        return false;
    }
    // Keep this optimization narrow: only inline when the scope output is
    // immediately forwarded into a local binding that is returned.
    let Some(scope_decl_id) = scope_block
        .scope
        .declarations
        .values()
        .next()
        .map(|decl| decl.identifier.declaration_id)
    else {
        return false;
    };
    if !matches_immediate_return_passthrough(scope_decl_id, following_stmts) {
        return false;
    }
    let Some(ReactiveStatement::Instruction(instr)) = scope_block.instructions.first() else {
        return false;
    };
    if instruction_has_autodeps_placeholder(cx, instr)
        || instruction_has_callback_and_array_args(cx, instr)
    {
        return false;
    }
    match &instr.value {
        InstructionValue::CallExpression { callee, args, .. } => {
            if !args.is_empty() {
                return false;
            }
            // Restrict to non-local/global callees to avoid inlining local
            // callback scopes (e.g. callback() wrappers that upstream memoizes).
            // Lowered imports often flow through LoadGlobal temporaries with
            // non-zero declaration ids, so rely on tracked origin metadata.
            if callee.identifier.declaration_id.0 != 0
                && !cx
                    .non_local_binding_decls
                    .contains(&callee.identifier.declaration_id)
            {
                return false;
            }
            let callee_name = resolve_place_name(cx, callee)
                .unwrap_or_else(|| codegen_place_to_expression(cx, callee));
            extract_hook_name(&callee_name).is_none()
        }
        _ => false,
    }
}

fn matches_immediate_return_passthrough(
    scope_output_decl: DeclarationId,
    following_stmts: &[ReactiveStatement],
) -> bool {
    if following_stmts.len() != 3 {
        return false;
    }

    let (
        ReactiveStatement::Instruction(store_instr),
        ReactiveStatement::Instruction(load_instr),
        ReactiveStatement::Terminal(term_stmt),
    ) = (
        &following_stmts[0],
        &following_stmts[1],
        &following_stmts[2],
    )
    else {
        return false;
    };

    let (store_target_decl, store_rhs_decl) = match &store_instr.value {
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            if lvalue.kind == InstructionKind::Reassign {
                return false;
            }
            (
                lvalue.place.identifier.declaration_id,
                value.identifier.declaration_id,
            )
        }
        _ => return false,
    };
    if store_rhs_decl != scope_output_decl {
        return false;
    }

    let load_lvalue_decl = match &load_instr.value {
        InstructionValue::LoadLocal { place, .. }
            if place.identifier.declaration_id == store_target_decl =>
        {
            load_instr
                .lvalue
                .as_ref()
                .map(|lvalue| lvalue.identifier.declaration_id)
        }
        _ => None,
    };
    let Some(load_lvalue_decl) = load_lvalue_decl else {
        return false;
    };

    matches!(
        &term_stmt.terminal,
        ReactiveTerminal::Return { value, .. } if value.identifier.declaration_id == load_lvalue_decl
    )
}

fn block_contains_autodeps_call_usage(
    block: &[ReactiveStatement],
    autodeps_decls: &mut HashSet<DeclarationId>,
) -> bool {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if instruction_contains_autodeps_call_usage(instr, autodeps_decls) {
                    return true;
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                let mut nested_autodeps = autodeps_decls.clone();
                if block_contains_autodeps_call_usage(
                    &scope_block.instructions,
                    &mut nested_autodeps,
                ) {
                    return true;
                }
                autodeps_decls.extend(nested_autodeps);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                let mut nested_autodeps = autodeps_decls.clone();
                if block_contains_autodeps_call_usage(
                    &scope_block.instructions,
                    &mut nested_autodeps,
                ) {
                    return true;
                }
                autodeps_decls.extend(nested_autodeps);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let mut nested_autodeps = autodeps_decls.clone();
                if terminal_contains_autodeps_call_usage(&term_stmt.terminal, &mut nested_autodeps)
                {
                    return true;
                }
                autodeps_decls.extend(nested_autodeps);
            }
        }
    }
    false
}

fn block_contains_hook_call(cx: &mut Context, block: &[ReactiveStatement]) -> bool {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if instruction_is_hook_call(cx, instr) {
                    return true;
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                if block_contains_hook_call(cx, &scope_block.instructions) {
                    return true;
                }
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                if block_contains_hook_call(cx, &scope_block.instructions) {
                    return true;
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                if terminal_contains_hook_call(cx, &term_stmt.terminal) {
                    return true;
                }
            }
        }
    }
    false
}

fn terminal_contains_hook_call(cx: &mut Context, terminal: &ReactiveTerminal) -> bool {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            block_contains_hook_call(cx, consequent)
                || alternate
                    .as_ref()
                    .is_some_and(|block| block_contains_hook_call(cx, block))
        }
        ReactiveTerminal::Switch { cases, .. } => cases
            .iter()
            .filter_map(|case| case.block.as_ref())
            .any(|block| block_contains_hook_call(cx, block)),
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => block_contains_hook_call(cx, loop_block),
        ReactiveTerminal::Try { block, handler, .. } => {
            block_contains_hook_call(cx, block) || block_contains_hook_call(cx, handler)
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => false,
    }
}

fn instruction_is_hook_call(cx: &mut Context, instr: &ReactiveInstruction) -> bool {
    match &instr.value {
        InstructionValue::CallExpression { callee, .. } => {
            let callee_name = resolve_place_name(cx, callee)
                .unwrap_or_else(|| codegen_place_to_expression(cx, callee));
            extract_hook_name(&callee_name).is_some()
        }
        InstructionValue::MethodCall {
            receiver, property, ..
        } => {
            let receiver_name = resolve_place_name(cx, receiver)
                .unwrap_or_else(|| codegen_place_to_expression(cx, receiver));
            let (property_name, _) = resolve_method_property(cx, property, &receiver_name);
            Environment::is_hook_name(&property_name)
        }
        _ => false,
    }
}

fn collect_declaration_assignment_edges_in_block(
    block: &[ReactiveStatement],
    out: &mut Vec<(DeclarationId, DeclarationId)>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => match &instr.value {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    out.push((
                        value.identifier.declaration_id,
                        lvalue.place.identifier.declaration_id,
                    ));
                }
                _ => {}
            },
            ReactiveStatement::Scope(scope_block) => {
                collect_declaration_assignment_edges_in_block(&scope_block.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_declaration_assignment_edges_in_block(&scope_block.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_declaration_assignment_edges_in_terminal(&term_stmt.terminal, out);
            }
        }
    }
}

fn collect_declaration_assignment_edges_in_terminal(
    terminal: &ReactiveTerminal,
    out: &mut Vec<(DeclarationId, DeclarationId)>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_declaration_assignment_edges_in_block(consequent, out);
            if let Some(alt) = alternate {
                collect_declaration_assignment_edges_in_block(alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_declaration_assignment_edges_in_block(block, out);
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_declaration_assignment_edges_in_block(loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_declaration_assignment_edges_in_block(block, out);
            collect_declaration_assignment_edges_in_block(handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_hook_callback_decl_ids_in_block(
    cx: &mut Context,
    block: &[ReactiveStatement],
    out: &mut HashSet<DeclarationId>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                collect_hook_callback_decl_ids_from_instruction(cx, instr, out);
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_hook_callback_decl_ids_in_block(cx, &scope_block.instructions, out);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_hook_callback_decl_ids_in_block(cx, &scope_block.instructions, out);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_hook_callback_decl_ids_in_terminal(cx, &term_stmt.terminal, out);
            }
        }
    }
}

fn collect_hook_callback_decl_ids_in_terminal(
    cx: &mut Context,
    terminal: &ReactiveTerminal,
    out: &mut HashSet<DeclarationId>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_hook_callback_decl_ids_in_block(cx, consequent, out);
            if let Some(alt) = alternate {
                collect_hook_callback_decl_ids_in_block(cx, alt, out);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_hook_callback_decl_ids_in_block(cx, block, out);
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_hook_callback_decl_ids_in_block(cx, loop_block, out);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_hook_callback_decl_ids_in_block(cx, block, out);
            collect_hook_callback_decl_ids_in_block(cx, handler, out);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_hook_callback_decl_ids_from_instruction(
    cx: &mut Context,
    instr: &ReactiveInstruction,
    out: &mut HashSet<DeclarationId>,
) {
    prime_hook_name_resolution_from_instruction(cx, instr);
    match &instr.value {
        InstructionValue::CallExpression { callee, args, .. } => {
            out.insert(callee.identifier.declaration_id);
            let is_hook_call = resolve_place_name(cx, callee)
                .is_some_and(|name| extract_hook_name(&name).is_some());
            if !is_hook_call {
                return;
            }
            for arg in args {
                if let Argument::Place(place) = arg {
                    out.insert(place.identifier.declaration_id);
                }
            }
        }
        InstructionValue::MethodCall { property, args, .. } => {
            let is_hook_call = resolve_place_name(cx, property)
                .is_some_and(|name| extract_hook_name(&name).is_some());
            if !is_hook_call {
                return;
            }
            for arg in args {
                if let Argument::Place(place) = arg {
                    out.insert(place.identifier.declaration_id);
                }
            }
        }
        InstructionValue::FunctionExpression { lowered_func, .. }
        | InstructionValue::ObjectMethod { lowered_func, .. } => {
            collect_invoked_capture_decls_from_lowered_function(lowered_func, out);
        }
        _ => {}
    }
}

fn collect_invoked_capture_decls_from_lowered_function(
    lowered_func: &LoweredFunction,
    out: &mut HashSet<DeclarationId>,
) {
    let mut captured_roots: HashSet<DeclarationId> = HashSet::new();
    for place in &lowered_func.func.context {
        captured_roots.insert(place.identifier.declaration_id);
    }
    if captured_roots.is_empty() {
        return;
    }

    let mut alias_to_capture: HashMap<DeclarationId, DeclarationId> = HashMap::new();
    for (_, block) in &lowered_func.func.body.blocks {
        for instr in &block.instructions {
            let lvalue_decl = instr.lvalue.identifier.declaration_id;
            match &instr.value {
                InstructionValue::LoadContext { place, .. } => {
                    let source_decl = place.identifier.declaration_id;
                    if captured_roots.contains(&source_decl) {
                        alias_to_capture.insert(lvalue_decl, source_decl);
                    }
                }
                InstructionValue::LoadLocal { place, .. }
                | InstructionValue::TypeCastExpression { value: place, .. } => {
                    if let Some(source_root) = alias_to_capture
                        .get(&place.identifier.declaration_id)
                        .copied()
                    {
                        alias_to_capture.insert(lvalue_decl, source_root);
                    }
                }
                _ => {}
            }

            if let InstructionValue::CallExpression { callee, .. } = &instr.value {
                let callee_decl = callee.identifier.declaration_id;
                if let Some(source_root) = alias_to_capture.get(&callee_decl).copied() {
                    out.insert(source_root);
                } else if captured_roots.contains(&callee_decl) {
                    out.insert(callee_decl);
                }
            }

            match &instr.value {
                InstructionValue::FunctionExpression { lowered_func, .. }
                | InstructionValue::ObjectMethod { lowered_func, .. } => {
                    collect_invoked_capture_decls_from_lowered_function(lowered_func, out);
                }
                _ => {}
            }
        }
    }
}

fn prime_hook_name_resolution_from_instruction(cx: &mut Context, instr: &ReactiveInstruction) {
    let Some(lvalue) = &instr.lvalue else {
        return;
    };
    match &instr.value {
        InstructionValue::LoadGlobal { binding, .. } => {
            cx.resolved_names
                .insert(lvalue.identifier.id, load_global_resolved_name(binding));
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            if let Some(name) = resolve_place_name(cx, place) {
                cx.resolved_names.insert(lvalue.identifier.id, name);
            }
        }
        InstructionValue::TypeCastExpression { value, .. } => {
            if let Some(name) = resolve_place_name(cx, value) {
                cx.resolved_names.insert(lvalue.identifier.id, name);
            }
        }
        InstructionValue::PropertyLoad {
            object,
            property: PropertyLiteral::String(prop),
            ..
        } => {
            if let Some(base) = resolve_place_name(cx, object) {
                cx.resolved_names
                    .insert(lvalue.identifier.id, format!("{base}.{prop}"));
            }
        }
        _ => {}
    }
}

fn terminal_contains_autodeps_call_usage(
    terminal: &ReactiveTerminal,
    autodeps_decls: &mut HashSet<DeclarationId>,
) -> bool {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            let mut conseq_autodeps = autodeps_decls.clone();
            if block_contains_autodeps_call_usage(consequent, &mut conseq_autodeps) {
                return true;
            }
            autodeps_decls.extend(conseq_autodeps);
            if let Some(alt) = alternate {
                let mut alt_autodeps = autodeps_decls.clone();
                if block_contains_autodeps_call_usage(alt, &mut alt_autodeps) {
                    return true;
                }
                autodeps_decls.extend(alt_autodeps);
            }
            false
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(case_block) = &case.block {
                    let mut case_autodeps = autodeps_decls.clone();
                    if block_contains_autodeps_call_usage(case_block, &mut case_autodeps) {
                        return true;
                    }
                    autodeps_decls.extend(case_autodeps);
                }
            }
            false
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => block_contains_autodeps_call_usage(loop_block, autodeps_decls),
        ReactiveTerminal::Try { block, handler, .. } => {
            let mut try_autodeps = autodeps_decls.clone();
            if block_contains_autodeps_call_usage(block, &mut try_autodeps) {
                return true;
            }
            let mut catch_autodeps = autodeps_decls.clone();
            if block_contains_autodeps_call_usage(handler, &mut catch_autodeps) {
                return true;
            }
            autodeps_decls.extend(try_autodeps);
            autodeps_decls.extend(catch_autodeps);
            false
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => false,
    }
}

fn instruction_contains_autodeps_call_usage(
    instr: &ReactiveInstruction,
    autodeps_decls: &mut HashSet<DeclarationId>,
) -> bool {
    let track_lvalue_if_autodeps = |autodeps_decls: &mut HashSet<DeclarationId>| {
        if let Some(lvalue) = &instr.lvalue {
            autodeps_decls.insert(lvalue.identifier.declaration_id);
        }
    };
    match &instr.value {
        InstructionValue::LoadGlobal { binding, .. } => {
            if non_local_binding_is_autodeps(binding) {
                track_lvalue_if_autodeps(autodeps_decls);
            }
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            if place_is_autodeps_value(place, autodeps_decls) {
                track_lvalue_if_autodeps(autodeps_decls);
            }
        }
        InstructionValue::StoreLocal { lvalue, value, .. }
        | InstructionValue::StoreContext { lvalue, value, .. } => {
            if place_is_autodeps_value(value, autodeps_decls) {
                autodeps_decls.insert(lvalue.place.identifier.declaration_id);
            }
        }
        InstructionValue::PropertyLoad {
            object, property, ..
        } => {
            if property_literal_is_autodeps(property)
                || place_is_autodeps_value(object, autodeps_decls)
            {
                track_lvalue_if_autodeps(autodeps_decls);
            }
        }
        InstructionValue::CallExpression { args, .. }
        | InstructionValue::MethodCall { args, .. } => {
            if args
                .iter()
                .any(|arg| argument_is_autodeps_value(arg, autodeps_decls))
            {
                return true;
            }
        }
        _ => {}
    }
    false
}

fn non_local_binding_is_autodeps(binding: &NonLocalBinding) -> bool {
    match binding {
        NonLocalBinding::ImportSpecifier { name, imported, .. } => {
            name == "AUTODEPS" || imported == "AUTODEPS"
        }
        NonLocalBinding::ImportDefault { name, .. }
        | NonLocalBinding::ImportNamespace { name, .. }
        | NonLocalBinding::ModuleLocal { name }
        | NonLocalBinding::Global { name } => name == "AUTODEPS",
    }
}

fn property_literal_is_autodeps(property: &PropertyLiteral) -> bool {
    matches!(property, PropertyLiteral::String(name) if name == "AUTODEPS")
}

fn argument_is_autodeps_value(arg: &Argument, autodeps_decls: &HashSet<DeclarationId>) -> bool {
    match arg {
        Argument::Place(place) | Argument::Spread(place) => {
            place_is_autodeps_value(place, autodeps_decls)
        }
    }
}

fn place_is_autodeps_value(place: &Place, autodeps_decls: &HashSet<DeclarationId>) -> bool {
    autodeps_decls.contains(&place.identifier.declaration_id)
        || place
            .identifier
            .name
            .as_ref()
            .is_some_and(|name| name.value() == "AUTODEPS")
}

fn is_codegen_temp_name(name: &str) -> bool {
    let trimmed = name.trim();
    if !trimmed.starts_with('t') {
        return false;
    }
    trimmed[1..].chars().all(|c| c.is_ascii_digit())
}

fn is_temp_like_identifier(_cx: &Context, id: &Identifier) -> bool {
    match &id.name {
        None => true,
        // Upstream codegen only treats unnamed identifiers as temporaries.
        // Promoted `tN` names are runtime bindings that earlier passes chose to
        // materialize, so re-inlining them here breaks evaluation order.
        Some(IdentifierName::Promoted(_)) | Some(IdentifierName::Named(_)) => false,
    }
}

fn parse_codegen_temp_index(name: &str) -> Option<u32> {
    let trimmed = name.trim();
    let suffix = trimmed.strip_prefix('t')?;
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok()
}

fn debug_codegen_expr(tag: &str, detail: impl AsRef<str>) {
    if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
        eprintln!("[CODEGEN_EXPR] {} {}", tag, detail.as_ref());
    }
}

fn instruction_value_tag(value: &InstructionValue) -> &'static str {
    match value {
        InstructionValue::Primitive { .. } => "Primitive",
        InstructionValue::LoadLocal { .. } => "LoadLocal",
        InstructionValue::LoadContext { .. } => "LoadContext",
        InstructionValue::LoadGlobal { .. } => "LoadGlobal",
        InstructionValue::StoreLocal { .. } => "StoreLocal",
        InstructionValue::StoreContext { .. } => "StoreContext",
        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
        InstructionValue::DeclareContext { .. } => "DeclareContext",
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::ReactiveSequenceExpression { .. } => "ReactiveSequenceExpression",
        InstructionValue::ReactiveOptionalExpression { .. } => "ReactiveOptionalExpression",
        InstructionValue::ReactiveLogicalExpression { .. } => "ReactiveLogicalExpression",
        InstructionValue::ReactiveConditionalExpression { .. } => "ReactiveConditionalExpression",
        InstructionValue::JsxExpression { .. } => "JsxExpression",
        InstructionValue::StartMemoize { .. } => "StartMemoize",
        InstructionValue::FinishMemoize { .. } => "FinishMemoize",
        _ => "Other",
    }
}

/// Check if a reactive block references an identifier with the given name.
/// Used to detect whether a catch body uses the original catch binding name.
fn handler_block_references_name(block: &ReactiveBlock, name: &str) -> bool {
    fn check_place(place: &Place, name: &str) -> bool {
        match &place.identifier.name {
            Some(IdentifierName::Named(n)) | Some(IdentifierName::Promoted(n)) => n == name,
            None => false,
        }
    }

    fn check_arg(arg: &Argument, name: &str) -> bool {
        match arg {
            Argument::Place(p) | Argument::Spread(p) => check_place(p, name),
        }
    }

    fn check_instr(instr: &ReactiveInstruction, name: &str) -> bool {
        // Check lvalue
        if let Some(ref lv) = instr.lvalue
            && check_place(lv, name)
        {
            return true;
        }
        // Check instruction value operands
        match &instr.value {
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => check_place(place, name),
            InstructionValue::StoreLocal { lvalue, value, .. } => {
                check_place(&lvalue.place, name) || check_place(value, name)
            }
            InstructionValue::CallExpression { callee, args, .. } => {
                check_place(callee, name) || args.iter().any(|a| check_arg(a, name))
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                ..
            } => {
                check_place(receiver, name)
                    || check_place(property, name)
                    || args.iter().any(|a| check_arg(a, name))
            }
            InstructionValue::PropertyLoad { object, .. } => check_place(object, name),
            InstructionValue::ArrayExpression { elements, .. } => {
                elements.iter().any(|e| match e {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => check_place(p, name),
                    ArrayElement::Hole => false,
                })
            }
            InstructionValue::LoadGlobal { binding, .. } => binding.name() == name,
            _ => false,
        }
    }

    fn check_block(block: &ReactiveBlock, name: &str) -> bool {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    if check_instr(instr, name) {
                        return true;
                    }
                }
                ReactiveStatement::Terminal(term_stmt) => {
                    if check_terminal(&term_stmt.terminal, name) {
                        return true;
                    }
                }
                ReactiveStatement::Scope(scope_block) => {
                    if check_block(&scope_block.instructions, name) {
                        return true;
                    }
                }
                ReactiveStatement::PrunedScope(pruned) => {
                    if check_block(&pruned.instructions, name) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn check_terminal(terminal: &ReactiveTerminal, name: &str) -> bool {
        match terminal {
            ReactiveTerminal::Return { value, .. } | ReactiveTerminal::Throw { value, .. } => {
                check_place(value, name)
            }
            ReactiveTerminal::If {
                consequent,
                alternate,
                ..
            } => {
                check_block(consequent, name)
                    || alternate.as_ref().is_some_and(|b| check_block(b, name))
            }
            ReactiveTerminal::Switch { cases, .. } => cases
                .iter()
                .any(|c| c.block.as_ref().is_some_and(|b| check_block(b, name))),
            ReactiveTerminal::For { loop_block, .. }
            | ReactiveTerminal::ForOf { loop_block, .. }
            | ReactiveTerminal::ForIn { loop_block, .. }
            | ReactiveTerminal::While { loop_block, .. }
            | ReactiveTerminal::DoWhile { loop_block, .. } => check_block(loop_block, name),
            ReactiveTerminal::Label { block, .. } => check_block(block, name),
            ReactiveTerminal::Try { block, handler, .. } => {
                check_block(block, name) || check_block(handler, name)
            }
            _ => false,
        }
    }

    check_block(block, name)
}

fn instruction_has_autodeps_placeholder(cx: &mut Context, instr: &ReactiveInstruction) -> bool {
    let args: &[Argument] = match &instr.value {
        InstructionValue::CallExpression { args, .. }
        | InstructionValue::MethodCall { args, .. } => args.as_slice(),
        _ => return false,
    };
    args.iter().map(|arg| codegen_argument(cx, arg)).any(|arg| {
        let trimmed = arg.trim();
        trimmed == "AUTODEPS" || trimmed.ends_with(".AUTODEPS")
    })
}

fn instruction_has_callback_and_array_args(cx: &mut Context, instr: &ReactiveInstruction) -> bool {
    let args: &[Argument] = match &instr.value {
        InstructionValue::CallExpression { args, .. }
        | InstructionValue::MethodCall { args, .. } => args.as_slice(),
        _ => return false,
    };
    let rendered_args: Vec<String> = args.iter().map(|arg| codegen_argument(cx, arg)).collect();
    let has_callback_arg = rendered_args.iter().any(|arg| {
        let trimmed = arg.trim();
        trimmed.contains("=>") || trimmed.starts_with("function")
    }) || args.iter().any(is_function_argument);
    let has_array_arg = rendered_args.iter().any(|arg| {
        let trimmed = arg.trim();
        trimmed.starts_with('[') && trimmed.ends_with(']')
    }) || args.iter().any(is_array_argument);
    has_callback_arg && has_array_arg
}

fn is_function_argument(arg: &Argument) -> bool {
    let place = match arg {
        Argument::Place(p) | Argument::Spread(p) => p,
    };
    matches!(place.identifier.type_, Type::Function { .. })
}

fn is_array_argument(arg: &Argument) -> bool {
    let place = match arg {
        Argument::Place(p) | Argument::Spread(p) => p,
    };
    matches!(
        &place.identifier.type_,
        Type::Object {
            shape_id: Some(shape),
        } if shape == BUILT_IN_ARRAY_ID
    )
}

fn memoizable_if_reassignment_scope_bridge<'a>(
    terminal: &'a ReactiveTerminal,
    following_stmts: &'a [ReactiveStatement],
) -> Option<(&'a Place, Identifier)> {
    let debug = std::env::var("DEBUG_SCOPE_BRIDGE").is_ok();

    let ReactiveTerminal::If {
        test,
        consequent,
        alternate: Some(alternate),
        ..
    } = terminal
    else {
        if debug {
            eprintln!("[SCOPE_BRIDGE] reject: terminal is not if-with-else");
        }
        return None;
    };

    let Some(cons_target) = extract_single_reassign_target(consequent) else {
        if debug {
            eprintln!("[SCOPE_BRIDGE] reject: consequent is not single reassign");
        }
        return None;
    };
    let Some(alt_target) = extract_single_reassign_target(alternate) else {
        if debug {
            eprintln!("[SCOPE_BRIDGE] reject: alternate is not single reassign");
        }
        return None;
    };
    if cons_target.declaration_id != alt_target.declaration_id {
        if debug {
            eprintln!(
                "[SCOPE_BRIDGE] reject: branch targets differ cons={} alt={}",
                cons_target.declaration_id.0, alt_target.declaration_id.0
            );
        }
        return None;
    }

    let mut next_scope = None;
    for stmt in following_stmts {
        match stmt {
            ReactiveStatement::Instruction(instr) if is_ignorable_bridge_interstitial(instr) => {}
            ReactiveStatement::Scope(scope_stmt) => {
                next_scope = Some(scope_stmt);
                break;
            }
            _ => break,
        }
    }
    let Some(next_scope) = next_scope else {
        if debug {
            eprintln!("[SCOPE_BRIDGE] reject: no reachable next scope");
        }
        return None;
    };
    let used_by_next_scope = next_scope.scope.dependencies.iter().any(|dep| {
        dep.path.is_empty() && dep.identifier.declaration_id == cons_target.declaration_id
    });
    if !used_by_next_scope {
        if debug {
            eprintln!(
                "[SCOPE_BRIDGE] reject: next scope does not depend on target decl={} name={}",
                cons_target.declaration_id.0,
                identifier_name_static(&cons_target)
            );
        }
        return None;
    }

    if debug {
        eprintln!(
            "[SCOPE_BRIDGE] accept: target decl={} name={} dep_count={}",
            cons_target.declaration_id.0,
            identifier_name_static(&cons_target),
            next_scope.scope.dependencies.len()
        );
    }

    Some((test, cons_target))
}

fn is_ignorable_bridge_interstitial(instr: &ReactiveInstruction) -> bool {
    matches!(
        instr.value,
        InstructionValue::DeclareLocal { .. }
            | InstructionValue::DeclareContext { .. }
            | InstructionValue::StartMemoize { .. }
            | InstructionValue::FinishMemoize { .. }
            | InstructionValue::LoadGlobal { .. }
            | InstructionValue::LoadLocal { .. }
            | InstructionValue::LoadContext { .. }
            | InstructionValue::Primitive { .. }
            | InstructionValue::TypeCastExpression { .. }
    )
}

fn extract_single_reassign_target(block: &ReactiveBlock) -> Option<Identifier> {
    let debug = std::env::var("DEBUG_SCOPE_BRIDGE").is_ok();
    if debug {
        eprintln!(
            "[SCOPE_BRIDGE] inspect branch block: {} statements",
            block.len()
        );
    }
    let mut target: Option<Identifier> = None;

    for (idx, stmt) in block.iter().enumerate() {
        if debug {
            let kind = match stmt {
                ReactiveStatement::Instruction(_) => "Instruction",
                ReactiveStatement::Scope(_) => "Scope",
                ReactiveStatement::PrunedScope(_) => "PrunedScope",
                ReactiveStatement::Terminal(_) => "Terminal",
            };
            eprintln!("[SCOPE_BRIDGE]   stmt[{idx}] kind={kind}");
        }
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        if debug {
            if let Some(lv) = &instr.lvalue {
                eprintln!(
                    "[SCOPE_BRIDGE]   - lvalue decl={} name={:?}",
                    lv.identifier.declaration_id.0, lv.identifier.name
                );
            } else {
                eprintln!("[SCOPE_BRIDGE]   - instruction has no outer lvalue");
            }
            eprintln!("[SCOPE_BRIDGE]   - value={:?}", instr.value);
        }
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. }
                if lvalue.kind == InstructionKind::Reassign =>
            {
                if let Some(existing) = &target {
                    if existing.declaration_id != lvalue.place.identifier.declaration_id {
                        if debug {
                            eprintln!(
                                "[SCOPE_BRIDGE]   - reject: conflicting reassignment target decl={} vs {}",
                                existing.declaration_id.0, lvalue.place.identifier.declaration_id.0
                            );
                        }
                        return None;
                    }
                } else {
                    target = Some(lvalue.place.identifier.clone());
                    if debug {
                        eprintln!(
                            "[SCOPE_BRIDGE]   - captured target decl={} name={}",
                            lvalue.place.identifier.declaration_id.0,
                            identifier_name_static(&lvalue.place.identifier)
                        );
                    }
                }
            }
            InstructionValue::StoreLocal { lvalue, .. }
            | InstructionValue::StoreContext { lvalue, .. } => {
                if lvalue.place.identifier.name.is_some()
                    && target
                        .as_ref()
                        .is_some_and(|t| t.declaration_id != lvalue.place.identifier.declaration_id)
                {
                    if debug {
                        eprintln!(
                            "[SCOPE_BRIDGE]   - reject: non-reassign store to different named decl={} target={}",
                            lvalue.place.identifier.declaration_id.0,
                            target
                                .as_ref()
                                .map(|t| t.declaration_id.0.to_string())
                                .unwrap_or_else(|| "?".to_string())
                        );
                    }
                    return None;
                }
            }
            _ => {
                // Reject writes to a different named declaration inside the branch.
                if instr
                    .lvalue
                    .as_ref()
                    .is_some_and(|lv| lv.identifier.name.is_some())
                    && target
                        .as_ref()
                        .zip(instr.lvalue.as_ref())
                        .is_some_and(|(t, lv)| t.declaration_id != lv.identifier.declaration_id)
                {
                    if debug {
                        let decl_id = instr
                            .lvalue
                            .as_ref()
                            .map(|lv| lv.identifier.declaration_id.0)
                            .unwrap_or(0);
                        eprintln!(
                            "[SCOPE_BRIDGE]   - reject: write to different named decl={} target={}",
                            decl_id,
                            target
                                .as_ref()
                                .map(|t| t.declaration_id.0.to_string())
                                .unwrap_or_else(|| "?".to_string())
                        );
                    }
                    return None;
                }
            }
        }
    }

    if debug {
        match &target {
            Some(t) => eprintln!(
                "[SCOPE_BRIDGE] branch target resolved decl={} name={}",
                t.declaration_id.0,
                identifier_name_static(t)
            ),
            None => eprintln!("[SCOPE_BRIDGE] branch target unresolved"),
        }
    }

    target
}

fn update_scope_line_span(
    loc: &SourceLocation,
    start_line: &mut Option<u32>,
    end_line: &mut Option<u32>,
) {
    let SourceLocation::Source(range) = loc else {
        return;
    };
    let (Some(start), Some(mut end)) = (if range.start.line == 0 && range.end.line == 0 {
        (
            crate::source_lines::line_from_offset(range.start.column),
            crate::source_lines::line_from_offset(range.end.column),
        )
    } else {
        (
            Some(range.start.line.saturating_add(1)),
            Some(range.end.line.saturating_add(1)),
        )
    }) else {
        return;
    };
    if end < start {
        end = start;
    }
    *start_line = Some(start_line.map_or(start, |line| line.min(start)));
    *end_line = Some(end_line.map_or(end, |line| line.max(end)));
}

fn collect_scope_line_span_from_terminal(
    terminal: &ReactiveTerminal,
    start_line: &mut Option<u32>,
    end_line: &mut Option<u32>,
) {
    match terminal {
        ReactiveTerminal::Break { loc, .. }
        | ReactiveTerminal::Continue { loc, .. }
        | ReactiveTerminal::Return { loc, .. }
        | ReactiveTerminal::Throw { loc, .. }
        | ReactiveTerminal::Switch { loc, .. }
        | ReactiveTerminal::DoWhile { loc, .. }
        | ReactiveTerminal::While { loc, .. }
        | ReactiveTerminal::For { loc, .. }
        | ReactiveTerminal::ForOf { loc, .. }
        | ReactiveTerminal::ForIn { loc, .. }
        | ReactiveTerminal::If { loc, .. }
        | ReactiveTerminal::Label { loc, .. }
        | ReactiveTerminal::Try { loc, .. } => update_scope_line_span(loc, start_line, end_line),
    }
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_scope_line_span_from_block(consequent, start_line, end_line);
            if let Some(alt) = alternate {
                collect_scope_line_span_from_block(alt, start_line, end_line);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_scope_line_span_from_block(block, start_line, end_line);
                }
            }
        }
        ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. } => {
            collect_scope_line_span_from_block(loop_block, start_line, end_line);
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_scope_line_span_from_block(init, start_line, end_line);
            if let Some(update_block) = update {
                collect_scope_line_span_from_block(update_block, start_line, end_line);
            }
            collect_scope_line_span_from_block(loop_block, start_line, end_line);
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        }
        | ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            collect_scope_line_span_from_block(init, start_line, end_line);
            collect_scope_line_span_from_block(loop_block, start_line, end_line);
        }
        ReactiveTerminal::Label { block, .. } => {
            collect_scope_line_span_from_block(block, start_line, end_line);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_scope_line_span_from_block(block, start_line, end_line);
            collect_scope_line_span_from_block(handler, start_line, end_line);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn collect_scope_line_span_from_block(
    block: &ReactiveBlock,
    start_line: &mut Option<u32>,
    end_line: &mut Option<u32>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                update_scope_line_span(&instr.loc, start_line, end_line);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_scope_line_span_from_terminal(&term_stmt.terminal, start_line, end_line);
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_scope_line_span_from_block(&scope_block.instructions, start_line, end_line);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_scope_line_span_from_block(&scope_block.instructions, start_line, end_line);
            }
        }
    }
}

fn format_change_detection_scope_loc(scope: &ReactiveScope, block: &ReactiveBlock) -> String {
    let mut start_line: Option<u32> = None;
    let mut end_line: Option<u32> = None;
    for decl in scope.declarations.values() {
        update_scope_line_span(&decl.identifier.loc, &mut start_line, &mut end_line);
    }
    for reassignment in &scope.reassignments {
        update_scope_line_span(&reassignment.loc, &mut start_line, &mut end_line);
    }
    collect_scope_line_span_from_block(block, &mut start_line, &mut end_line);
    if let (Some(start), Some(end)) = (start_line, end_line) {
        format!("({}:{})", start, end)
    } else {
        "unknown location".to_string()
    }
}

fn debug_loc(loc: &SourceLocation) -> String {
    match loc {
        SourceLocation::Source(range) => format!(
            "{}:{}-{}:{}",
            range.start.line, range.start.column, range.end.line, range.end.column
        ),
        SourceLocation::Generated => "generated".to_string(),
    }
}

fn is_readonly_console_callee(callee: &str) -> bool {
    callee.starts_with("console.") || callee.starts_with("global.console.")
}

fn split_top_level_call_args(args: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut paren_depth = 0i32;
    let mut bracket_depth = 0i32;
    let mut brace_depth = 0i32;
    for (idx, ch) in args.char_indices() {
        match ch {
            '(' => paren_depth += 1,
            ')' => paren_depth -= 1,
            '[' => bracket_depth += 1,
            ']' => bracket_depth -= 1,
            '{' => brace_depth += 1,
            '}' => brace_depth -= 1,
            ',' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                parts.push(args[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(args[start..].trim());
    parts
}

fn strip_wrapping_parens(mut expr: &str) -> &str {
    loop {
        let trimmed = expr.trim();
        if trimmed.len() < 2 || !trimmed.starts_with('(') || !trimmed.ends_with(')') {
            return trimmed;
        }
        let mut depth = 0i32;
        let mut wraps_entire_expr = true;
        for (idx, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 && idx + ch.len_utf8() < trimmed.len() {
                        wraps_entire_expr = false;
                        break;
                    }
                }
                _ => {}
            }
            if depth < 0 {
                wraps_entire_expr = false;
                break;
            }
        }
        if !wraps_entire_expr || depth != 0 {
            return trimmed;
        }
        expr = &trimmed[1..trimmed.len() - 1];
    }
}

fn is_readonly_output_call_line(line: &str, output_names: &HashSet<String>) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.ends_with(");") {
        return false;
    }
    if trimmed.starts_with("if ")
        || trimmed.starts_with("for ")
        || trimmed.starts_with("while ")
        || trimmed.starts_with("switch ")
        || trimmed.starts_with("try ")
        || trimmed.starts_with("return ")
        || trimmed.starts_with("throw ")
        || trimmed.contains(" = ")
    {
        return false;
    }

    let Some(open_paren) = trimmed.find('(') else {
        return false;
    };
    let callee = trimmed[..open_paren].trim();
    if !is_readonly_console_callee(callee) {
        return false;
    }

    let args = &trimmed[open_paren + 1..trimmed.len() - 2];
    let args = split_top_level_call_args(args);
    args.iter().any(|arg| {
        let normalized = strip_wrapping_parens(arg);
        output_names.contains(normalized)
    })
}

fn split_deferred_readonly_output_calls(
    computation: &str,
    output_names: &HashSet<String>,
) -> (String, Vec<String>) {
    let mut retained_lines: Vec<String> = Vec::new();
    let mut deferred_calls: Vec<String> = Vec::new();

    for line in computation.lines() {
        if is_readonly_output_call_line(line, output_names) {
            deferred_calls.push(format!("{}\n", line.trim()));
        } else {
            retained_lines.push(format!("{line}\n"));
        }
    }

    (retained_lines.concat(), deferred_calls)
}

/// Generate a reactive scope (memoization block).
///
/// Emits: `if ($[n] !== dep || ...) { ...compute... $[n] = dep; $[m] = output; } else { output = $[m]; }`
fn codegen_reactive_scope(
    cx: &mut Context,
    output: &mut String,
    scope: &ReactiveScope,
    block: &ReactiveBlock,
) {
    if std::env::var("DEBUG_REACTIVE_SCOPE_INFO").is_ok() {
        eprintln!(
            "[REACTIVE_SCOPE] scope={} range=({},{})",
            scope.id.0, scope.range.start.0, scope.range.end.0
        );
        if !scope.dependencies.is_empty() {
            eprintln!("[REACTIVE_SCOPE] deps:");
            for dep in &scope.dependencies {
                eprintln!(
                    "  - id={} decl={} name={:?} path={:?}",
                    dep.identifier.id.0,
                    dep.identifier.declaration_id.0,
                    dep.identifier.name,
                    dep.path
                );
            }
        }
        if !scope.declarations.is_empty() {
            eprintln!("[REACTIVE_SCOPE] declarations:");
            for (id, decl) in &scope.declarations {
                eprintln!(
                    "  - key_id={} decl_id={} name={:?} scope={} loc={}",
                    id.0,
                    decl.identifier.declaration_id.0,
                    decl.identifier.name,
                    decl.scope.id.0,
                    debug_loc(&decl.identifier.loc)
                );
            }
        }
        if !scope.reassignments.is_empty() {
            eprintln!("[REACTIVE_SCOPE] reassignments:");
            for id in &scope.reassignments {
                eprintln!(
                    "  - id={} decl={} name={:?} loc={}",
                    id.id.0,
                    id.declaration_id.0,
                    id.name,
                    debug_loc(&id.loc)
                );
            }
        }
        if !block.is_empty() {
            eprintln!("[REACTIVE_SCOPE] block_locs:");
            for stmt in block {
                match stmt {
                    ReactiveStatement::Instruction(instr) => {
                        eprintln!(
                            "  - instr id={} loc={} kind={:?}",
                            instr.id.0,
                            debug_loc(&instr.loc),
                            instr.value
                        );
                    }
                    ReactiveStatement::Terminal(term_stmt) => {
                        let loc = match &term_stmt.terminal {
                            ReactiveTerminal::Break { loc, .. }
                            | ReactiveTerminal::Continue { loc, .. }
                            | ReactiveTerminal::Return { loc, .. }
                            | ReactiveTerminal::Throw { loc, .. }
                            | ReactiveTerminal::Switch { loc, .. }
                            | ReactiveTerminal::DoWhile { loc, .. }
                            | ReactiveTerminal::While { loc, .. }
                            | ReactiveTerminal::For { loc, .. }
                            | ReactiveTerminal::ForOf { loc, .. }
                            | ReactiveTerminal::ForIn { loc, .. }
                            | ReactiveTerminal::If { loc, .. }
                            | ReactiveTerminal::Label { loc, .. }
                            | ReactiveTerminal::Try { loc, .. } => loc,
                        };
                        eprintln!("  - terminal loc={}", debug_loc(loc));
                    }
                    ReactiveStatement::Scope(scope_block) => {
                        eprintln!("  - nested scope id={}", scope_block.scope.id.0);
                    }
                    ReactiveStatement::PrunedScope(pruned) => {
                        eprintln!("  - nested pruned scope id={}", pruned.scope.id.0);
                    }
                }
            }
        }
    }

    let cache_var = cx.synthesize_name("$");
    let mut change_exprs: Vec<String> = Vec::new();
    let mut change_var_stmts: Vec<String> = Vec::new();
    let mut cache_store_stmts: Vec<String> = Vec::new();
    let mut cache_load_stmts: Vec<String> = Vec::new();

    // Collect callback deps for function expressions in this scope block before
    // generating the guard, since `codegen_block` (which also records callback
    // deps) runs after dependency guards are emitted.
    let mut hook_callback_decl_ids: HashSet<DeclarationId> = HashSet::new();
    let scope_decl_ids: HashSet<DeclarationId> = scope
        .declarations
        .values()
        .map(|decl| decl.identifier.declaration_id)
        .collect();
    for decl in scope.declarations.values() {
        let decl_id = decl.identifier.declaration_id;
        if cx.hook_callback_arg_decls.contains(&decl_id) {
            hook_callback_decl_ids.insert(decl_id);
        }
    }
    for stmt in block {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, value, .. }
            | InstructionValue::StoreContext { lvalue, value, .. } => {
                let source_decl = value.identifier.declaration_id;
                if !scope_decl_ids.contains(&source_decl) {
                    continue;
                }
                let target_decl = lvalue.place.identifier.declaration_id;
                if cx.hook_callback_arg_decls.contains(&target_decl) {
                    hook_callback_decl_ids.insert(source_decl);
                }
            }
            _ => {}
        }
    }
    for reassignment in &scope.reassignments {
        if cx
            .hook_callback_arg_decls
            .contains(&reassignment.declaration_id)
        {
            hook_callback_decl_ids.insert(reassignment.declaration_id);
        }
    }
    let mut scope_callback_deps: HashMap<DeclarationId, Vec<String>> = HashMap::new();
    if !hook_callback_decl_ids.is_empty() {
        let primitive_literals_for_callbacks = cx.primitive_literals_for_child();
        collect_callback_deps_from_reactive_block(
            block,
            &cx.stable_ref_decls,
            &cx.stable_setter_decls,
            &cx.stable_effect_event_decls,
            &cx.multi_source_decls,
            &primitive_literals_for_callbacks,
            &mut scope_callback_deps,
        );
    }

    // Dependencies: each gets a cache slot, generate change checks and store statements.
    // Truncate ref.current deps to just ref (upstream: PropagateScopeDependenciesHIR.ts L610-620).
    let drop_deps_for_single_iteration_do_while = scope.dependencies.len() == 1
        && scope.reassignments.is_empty()
        && scope.declarations.len() == 1
        && is_single_iteration_do_while_scope_block(block);
    let truncated_deps: Vec<ReactiveScopeDependency> = if drop_deps_for_single_iteration_do_while {
        Vec::new()
    } else {
        scope
            .dependencies
            .iter()
            .map(|dep| truncate_ref_current_dep(dep, &cx.stable_ref_decls))
            .collect()
    };
    let mut sorted_deps: Vec<&ReactiveScopeDependency> = truncated_deps.iter().collect();
    sort_scope_dependency_refs_for_codegen(cx, &mut sorted_deps);
    let manual_memo_dep_roots_for_scope: HashSet<DeclarationId> = scope
        .declarations
        .values()
        .flat_map(|decl| {
            cx.manual_memo_dep_roots_by_decl
                .get(&decl.identifier.declaration_id)
                .into_iter()
                .flat_map(|roots| roots.iter().copied())
        })
        .collect();
    let mut seen_dep_keys: HashSet<String> = HashSet::new();
    let mut scope_dep_exprs: Vec<String> = Vec::new();
    let debug_scope_dep_filter = std::env::var("DEBUG_SCOPE_DEP_FILTER").is_ok();
    for dep in sorted_deps {
        if dep.path.is_empty()
            && cx
                .stable_setter_decls
                .contains(&dep.identifier.declaration_id)
            && !cx
                .multi_source_decls
                .contains(&dep.identifier.declaration_id)
            && !manual_memo_dep_roots_for_scope.contains(&dep.identifier.declaration_id)
            && !cx
                .pending_manual_memo_reads
                .contains(&dep.identifier.declaration_id)
        {
            if debug_scope_dep_filter {
                eprintln!(
                    "[SCOPE_DEP_FILTER] skip=stable_setter scope={} decl={} id={} name={:?} path={:?}",
                    scope.id.0,
                    dep.identifier.declaration_id.0,
                    dep.identifier.id.0,
                    dep.identifier.name,
                    dep.path
                );
            }
            continue;
        }
        let key = format_dependency_identity(dep);
        if seen_dep_keys.insert(key) {
            scope_dep_exprs.push(codegen_dependency(cx, dep));
        }
    }
    if block_has_optional_computed_load_call_key(block) {
        let mut widened: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for dep in scope_dep_exprs {
            let dep = widen_member_dep_expr_to_root(&dep).unwrap_or(dep);
            if seen.insert(dep.clone()) {
                widened.push(dep);
            }
        }
        scope_dep_exprs = widened;
    }
    // Do NOT use infer_fallback_scope_dep_exprs here.
    // When scope deps are empty (pruned by prune_non_reactive_deps_reactive),
    // the scope should be sentinel-based. Re-inferring deps from JSX props
    // would re-add non-reactive deps that were intentionally pruned.
    //
    // Upstream note:
    // Guard deps come from scope deps; callback deps only provide an override
    // when callback-capture metadata is strictly more precise for this scope.
    let mut hook_callback_decl_ids_sorted: Vec<DeclarationId> =
        hook_callback_decl_ids.into_iter().collect();
    hook_callback_decl_ids_sorted.sort_by_key(|id| id.0);
    let mut callback_dep_exprs: Vec<String> = Vec::new();
    for decl_id in &hook_callback_decl_ids_sorted {
        if let Some(deps) = cx
            .callback_deps
            .get(decl_id)
            .or_else(|| scope_callback_deps.get(decl_id))
        {
            callback_dep_exprs.extend(deps.iter().cloned());
        }
    }
    callback_dep_exprs = dedupe_dependency_paths(callback_dep_exprs);
    let override_dep_exprs = if std::env::var("DISABLE_CALLBACK_DEP_OVERRIDE").is_ok() {
        Vec::new()
    } else if !callback_dep_exprs.is_empty() {
        choose_callback_dep_override(&scope_dep_exprs, &callback_dep_exprs)
    } else {
        Vec::new()
    };
    if std::env::var("DEBUG_SCOPE_DEP_OVERRIDE").is_ok() {
        let mut scope_callback_keys: Vec<u32> =
            scope_callback_deps.keys().map(|decl| decl.0).collect();
        scope_callback_keys.sort_unstable();
        let hook_decl_ids: Vec<u32> = hook_callback_decl_ids_sorted
            .iter()
            .map(|id| id.0)
            .collect();
        eprintln!(
            "[SCOPE_DEP_OVERRIDE] scope={} hook_decl_ids={:?} scope_callback_keys={:?} scope_deps={:?} callback_deps={:?} override={:?}",
            scope.id.0,
            hook_decl_ids,
            scope_callback_keys,
            scope_dep_exprs,
            callback_dep_exprs,
            override_dep_exprs
        );
    }

    let reassigned_decl_ids: HashSet<DeclarationId> = scope
        .reassignments
        .iter()
        .map(|id| id.declaration_id)
        .collect();
    // Declarations: each gets a cache slot
    let mut sorted_decls: Vec<(&IdentifierId, &ScopeDeclaration)> =
        scope.declarations.iter().collect();
    sorted_decls.sort_by(|a, b| {
        let a_reassigned = reassigned_decl_ids.contains(&a.1.identifier.declaration_id);
        let b_reassigned = reassigned_decl_ids.contains(&b.1.identifier.declaration_id);
        match a_reassigned.cmp(&b_reassigned) {
            std::cmp::Ordering::Equal => {}
            non_eq => return non_eq,
        }
        let a_name = identifier_name_static(&a.1.identifier);
        let b_name = identifier_name_static(&b.1.identifier);
        match a_name.cmp(&b_name) {
            std::cmp::Ordering::Equal => {
                a.1.identifier
                    .declaration_id
                    .0
                    .cmp(&b.1.identifier.declaration_id.0)
            }
            non_eq => non_eq,
        }
    });

    if !override_dep_exprs.is_empty() {
        for decl_id in &hook_callback_decl_ids_sorted {
            if cx.callback_deps.contains_key(decl_id) || scope_callback_deps.contains_key(decl_id) {
                cx.callback_deps
                    .insert(*decl_id, override_dep_exprs.clone());
            }
        }
    }

    let mut selected_dep_exprs = if !override_dep_exprs.is_empty() {
        override_dep_exprs.clone()
    } else {
        scope_dep_exprs.clone()
    };
    if block.len() == 1 && selected_dep_exprs.iter().all(|dep| !dep.contains("?.")) {
        let fallback_dep_exprs = infer_fallback_scope_dep_exprs(cx, block);
        if fallback_dep_exprs.iter().any(|dep| dep.contains("?.")) {
            selected_dep_exprs =
                replace_dep_exprs_with_optional_fallbacks(selected_dep_exprs, &fallback_dep_exprs);
        }
    }
    let output_decl_is_scope_dependency = sorted_decls.first().is_some_and(|(_, decl)| {
        cx.scope_dependency_decls
            .contains(&decl.identifier.declaration_id)
    });
    let should_dememoize_zero_dep_call_scope = selected_dep_exprs.is_empty()
        && scope.reassignments.is_empty()
        && scope.declarations.len() == 1
        && block.len() == 1
        && matches!(
            &block[0],
            ReactiveStatement::Instruction(instr)
                if matches!(
                    &instr.value,
                    InstructionValue::CallExpression {
                        callee,
                        args,
                        optional,
                        ..
                    }
                        if cx
                            .hook_call_by_decl
                            .get(&callee.identifier.declaration_id)
                            .is_some_and(|hook| hook == "useCallback")
                            || (output_decl_is_scope_dependency
                                && !optional
                                && args.is_empty()
                                && matches!(callee.effect, Effect::Read)
                                && !callee.reactive)
                )
        );
    if should_dememoize_zero_dep_call_scope {
        output.push_str(&codegen_scope_computation_no_reset(cx, scope, block));
        for decl in scope.declarations.values() {
            cx.stable_zero_dep_decls
                .insert(decl.identifier.declaration_id);
        }
        return;
    }
    let mut optional_dep_alias: Option<(String, String)> = None;
    let single_decl_is_function_like = matches!(
        block.first(),
        Some(ReactiveStatement::Instruction(instr))
            if matches!(
                &instr.value,
                InstructionValue::FunctionExpression { .. } | InstructionValue::ObjectMethod { .. }
            )
    );
    if sorted_decls.len() == 1
        && scope.reassignments.is_empty()
        && block.len() == 1
        && !single_decl_is_function_like
        && let Some(dep_index) = selected_dep_exprs.iter().position(|dep| dep.contains("?."))
    {
        let dep_expr = selected_dep_exprs[dep_index].clone();
        let decl = sorted_decls[0].1;
        let alias_name = identifier_name_with_cx(cx, &decl.identifier);
        if let Some(alias_index) = parse_codegen_temp_index(&alias_name) {
            let mut shifted_index = alias_index + 1;
            let mut shifted_name = format!("t{}", shifted_index);
            while shifted_name == alias_name
                || cx.used_declaration_names.contains(&shifted_name)
                || cx.reserved_child_decl_names.contains(&shifted_name)
            {
                shifted_index += 1;
                shifted_name = format!("t{}", shifted_index);
            }
            cx.declaration_name_overrides
                .insert(decl.identifier.declaration_id, shifted_name.clone());
            cx.used_declaration_names.insert(shifted_name.clone());
            cx.unique_identifiers.insert(shifted_name);

            if let Some(stmt) = render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Const,
                &alias_name,
                Some(&dep_expr),
            ) {
                output.push_str(&stmt);
            } else {
                output.push_str(&format!("const {} = {};\n", alias_name, dep_expr));
            }
            selected_dep_exprs[dep_index] = alias_name.clone();
            optional_dep_alias = Some((dep_expr, alias_name));
        }
    }
    for dep_expr in &selected_dep_exprs {
        let index = cx.alloc_cache_slot();
        let comparison = format!("{}[{}] !== {}", cache_var, index, dep_expr);
        if cx.enable_change_variable_codegen {
            let change_name = cx.synthesize_name(&format!("c_{}", index));
            change_var_stmts.push(
                render_reactive_variable_statement_ast(
                    ast::VariableDeclarationKind::Let,
                    &change_name,
                    Some(&comparison),
                )
                .unwrap_or_else(|| format!("let {} = {};\n", change_name, comparison)),
            );
            change_exprs.push(change_name);
        } else {
            change_exprs.push(comparison);
        }
        cache_store_stmts.push(
            render_reactive_expression_statement_ast(&format!(
                "{}[{}] = {}",
                cache_var, index, dep_expr
            ))
            .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, index, dep_expr)),
        );
    }
    let active_dep_exprs: HashSet<String> = selected_dep_exprs.iter().cloned().collect();

    for stmt in &change_var_stmts {
        output.push_str(stmt);
    }

    let mut first_output_index: Option<u32> = None;
    let mut cache_loads: Vec<(String, u32)> = Vec::new();
    let mut seen_output_names: HashSet<String> = HashSet::new();

    for (_, decl) in &sorted_decls {
        let mut name = identifier_name_with_cx(cx, &decl.identifier);
        let name_in_current_block = cx
            .block_scope_output_names
            .last()
            .is_some_and(|names| names.contains(&name));
        if is_codegen_temp_name(&name)
            && (active_dep_exprs.contains(&name)
                || (!has_materialized_named_binding(cx, &decl.identifier) && name_in_current_block))
        {
            // Avoid `tN` output names that collide with dependency expressions in
            // the same guard, e.g. `if ($[0] !== t0) { t0 = ... }`.
            // Also avoid reusing an already-emitted `tN` scope output binding in
            // the current lexical block.
            let fresh = fresh_temp_name(cx);
            cx.declaration_name_overrides
                .insert(decl.identifier.declaration_id, fresh.clone());
            cx.used_declaration_names.insert(fresh.clone());
            name = fresh;
        }
        if std::env::var("DEBUG_REACTIVE_SCOPE_NAMES").is_ok() {
            eprintln!(
                "[REACTIVE_SCOPE_NAME] fn={} scope={} decl_id={} ident_id={} ident_name={:?} emitted={}",
                cx.function_name,
                scope.id.0,
                decl.identifier.declaration_id.0,
                decl.identifier.id.0,
                decl.identifier.name,
                name
            );
        }
        if !seen_output_names.insert(name.clone()) {
            cx.declare(&decl.identifier);
            continue;
        }
        let index = cx.alloc_cache_slot();
        if first_output_index.is_none() {
            first_output_index = Some(index);
        }
        if !has_materialized_named_binding(cx, &decl.identifier) {
            if let Some(stmt) = render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Let,
                &name,
                None,
            ) {
                output.push_str(&stmt);
            } else {
                output.push_str(&format!("let {};\n", name));
            }
            if let Some(names) = cx.block_scope_output_names.last_mut() {
                names.insert(name.clone());
            }
            cx.mark_decl_runtime_emitted(decl.identifier.declaration_id);
        }
        cache_loads.push((name.clone(), index));
        cx.declare(&decl.identifier);
    }

    // Reassignments
    for reassignment in &scope.reassignments {
        let name = identifier_name_with_cx(cx, reassignment);
        if std::env::var("DEBUG_REACTIVE_SCOPE_NAMES").is_ok() {
            eprintln!(
                "[REACTIVE_SCOPE_NAME] fn={} scope={} reassignment_decl_id={} ident_id={} ident_name={:?} emitted={}",
                cx.function_name,
                scope.id.0,
                reassignment.declaration_id.0,
                reassignment.id.0,
                reassignment.name,
                name
            );
        }
        if !seen_output_names.insert(name.clone()) {
            continue;
        }
        let index = cx.alloc_cache_slot();
        if first_output_index.is_none() {
            first_output_index = Some(index);
        }
        cache_loads.push((name, index));
    }

    // Build test condition
    let mut test_condition = if change_exprs.is_empty() {
        // No dependencies - check if first output is sentinel
        if let Some(first_idx) = first_output_index {
            format!(
                "{}[{}] === Symbol.for(\"{}\")",
                cache_var, first_idx, MEMO_CACHE_SENTINEL
            )
        } else {
            "true".to_string()
        }
    } else {
        change_exprs.join(" || ")
    };
    if cx.disable_memoization_for_debugging {
        test_condition = format!("{} || true", test_condition);
    }

    // Generate computation block
    // Scope computations are emitted inside this scope's cache-guard body.
    // Keep their temporary output name tracking isolated from the surrounding
    // lexical block so inner `let tN` declarations do not force unrelated
    // outer-scope renames.
    cx.block_scope_output_names.push(HashSet::new());
    cx.block_scope_declared_temp_names.push(HashSet::new());
    let mut computation = codegen_scope_computation_no_reset(cx, scope, block);
    let _ = cx.block_scope_declared_temp_names.pop();
    let _ = cx.block_scope_output_names.pop();
    computation = rewrite_named_test_reassign_ternary_in_scope_computation(&computation);
    computation = rewrite_named_temp_ternary_in_scope_computation(&computation);
    if let Some((dep_expr, alias)) = &optional_dep_alias {
        computation = computation.replace(dep_expr, alias);
    }
    let mut deferred_post_scope_calls: Vec<String> = Vec::new();
    let block_is_flat_instructions = block
        .iter()
        .all(|stmt| matches!(stmt, ReactiveStatement::Instruction(_)));
    if block_is_flat_instructions && !cache_loads.is_empty() {
        let output_names: HashSet<String> =
            cache_loads.iter().map(|(name, _)| name.clone()).collect();
        let (next_computation, deferred_calls) =
            split_deferred_readonly_output_calls(&computation, &output_names);
        if !deferred_calls.is_empty() {
            computation = next_computation;
            deferred_post_scope_calls = deferred_calls;
        }
    }

    // Build cache store statements for outputs
    for (name, index) in &cache_loads {
        cache_store_stmts.push(
            render_reactive_expression_statement_ast(&format!(
                "{}[{}] = {}",
                cache_var, index, name
            ))
            .unwrap_or_else(|| format!("{}[{}] = {};\n", cache_var, index, name)),
        );
    }

    // Build cache load statements for outputs
    for (name, index) in &cache_loads {
        cache_load_stmts.push(
            render_reactive_assignment_statement_ast(name, &format!("{}[{}]", cache_var, index))
                .unwrap_or_else(|| format!("{} = {}[{}];\n", name, cache_var, index)),
        );
    }

    // Emit debug change detection scope form (upstream: enableChangeDetectionForDebugging).
    if cx.enable_change_detection_for_debugging && !change_exprs.is_empty() {
        cx.needs_structural_check_import = true;
        let condition_name = cx.synthesize_name("condition");
        let scope_loc = format_change_detection_scope_loc(scope, block);
        output.push_str("{\n");
        output.push_str(&computation);
        output.push_str(
            &render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Let,
                &condition_name,
                Some(&test_condition),
            )
            .unwrap_or_else(|| format!("let {} = {};\n", condition_name, test_condition)),
        );
        let mut cached_body = String::new();
        for (name, index) in &cache_loads {
            let old_name = cx.synthesize_name(&format!("old${}", name));
            cached_body.push_str(
                &render_reactive_variable_statement_ast(
                    ast::VariableDeclarationKind::Let,
                    &old_name,
                    Some(&format!("{}[{}]", cache_var, index)),
                )
                .unwrap_or_else(|| format!("let {} = {}[{}];\n", old_name, cache_var, index)),
            );
            cached_body.push_str(
                &render_reactive_expression_statement_ast(&format!(
                    "$structuralCheck({}, {}, \"{}\", \"{}\", \"cached\", \"{}\")",
                    old_name, name, name, cx.function_name, scope_loc
                ))
                .unwrap_or_else(|| {
                    format!(
                        "$structuralCheck({}, {}, \"{}\", \"{}\", \"cached\", \"{}\");\n",
                        old_name, name, name, cx.function_name, scope_loc
                    )
                }),
            );
        }
        output.push_str(
            &render_reactive_if_statement_ast(&format!("!{}", condition_name), &cached_body, None)
                .unwrap_or_else(|| format!("if (!{}) {{\n{}}}\n", condition_name, cached_body)),
        );
        for stmt in &cache_store_stmts {
            output.push_str(stmt);
        }
        let mut recomputed_body = computation.clone();
        for (name, index) in &cache_loads {
            recomputed_body.push_str(
                &render_reactive_expression_statement_ast(&format!(
                    "$structuralCheck({}[{}], {}, \"{}\", \"{}\", \"recomputed\", \"{}\")",
                    cache_var, index, name, name, cx.function_name, scope_loc
                ))
                .unwrap_or_else(|| {
                    format!(
                        "$structuralCheck({}[{}], {}, \"{}\", \"{}\", \"recomputed\", \"{}\");\n",
                        cache_var, index, name, name, cx.function_name, scope_loc
                    )
                }),
            );
            recomputed_body.push_str(
                &render_reactive_assignment_statement_ast(
                    name,
                    &format!("{}[{}]", cache_var, index),
                )
                .unwrap_or_else(|| format!("{} = {}[{}];\n", name, cache_var, index)),
            );
        }
        output.push_str(
            &render_reactive_if_statement_ast(&condition_name, &recomputed_body, None)
                .unwrap_or_else(|| format!("if ({}) {{\n{}}}\n", condition_name, recomputed_body)),
        );
        output.push_str("}\n");
    } else {
        // Emit the standard if/else memoization guard.
        let mut consequent = computation.clone();
        for stmt in &cache_store_stmts {
            consequent.push_str(stmt);
        }
        let alternate = cache_load_stmts.concat();
        output.push_str(
            &render_reactive_if_statement_ast(&test_condition, &consequent, Some(&alternate))
                .unwrap_or_else(|| {
                    let multiline_guard = !change_exprs.is_empty()
                        && change_exprs.len() > 1
                        && format!("if ({}) {{", test_condition).len() > 80;
                    let mut fallback = String::new();
                    if multiline_guard {
                        fallback.push_str("if (\n");
                        for (index, expr) in change_exprs.iter().enumerate() {
                            if index + 1 < change_exprs.len() {
                                fallback.push_str(&format!("{} ||\n", expr));
                            } else {
                                fallback.push_str(&format!("{}\n", expr));
                            }
                        }
                        fallback.push_str(") {\n");
                    } else {
                        fallback.push_str(&format!("if ({}) {{\n", test_condition));
                    }
                    fallback.push_str(&consequent);
                    fallback.push_str("} else {\n");
                    fallback.push_str(&alternate);
                    fallback.push_str("}\n");
                    fallback
                }),
        );
    }
    for stmt in &deferred_post_scope_calls {
        output.push_str(stmt);
    }

    if selected_dep_exprs.is_empty()
        && scope.reassignments.is_empty()
        && block.len() == 1
        && let ReactiveStatement::Instruction(instr) = &block[0]
        && matches!(
            instr.value,
            InstructionValue::FunctionExpression { .. }
                | InstructionValue::ObjectMethod { .. }
                | InstructionValue::Primitive { .. }
                | InstructionValue::ArrayExpression { .. }
                | InstructionValue::ObjectExpression { .. }
        )
    {
        for (_, decl) in &sorted_decls {
            cx.stable_zero_dep_decls
                .insert(decl.identifier.declaration_id);
        }
    }

    // Early return value
    if let Some(early_return) = &scope.early_return_value {
        let name = identifier_name_with_cx(cx, &early_return.value);
        let consequent = render_reactive_return_statement_ast(Some(&name))
            .unwrap_or_else(|| format!("return {};\n", name));
        output.push_str(
            &render_reactive_if_statement_ast(
                &format!("{} !== Symbol.for(\"{}\")", name, EARLY_RETURN_SENTINEL),
                &consequent,
                None,
            )
            .unwrap_or_else(|| {
                format!(
                    "if ({} !== Symbol.for(\"{}\")) {{\nreturn {};\n}}\n",
                    name, EARLY_RETURN_SENTINEL, name
                )
            }),
        );
    }
}

/// Generate a terminal statement.
fn codegen_terminal(cx: &mut Context, terminal: &ReactiveTerminal) -> Option<String> {
    fn extract_single_labeled_break_target(code: &str) -> Option<String> {
        let line = code.trim();
        if line.starts_with("break bb") && line.ends_with(';') && !line.contains('\n') {
            Some(line.to_string())
        } else {
            None
        }
    }

    fn trim_trailing_labeled_break_if_matches_default(
        current_case_code: &str,
        next_case_is_default: bool,
        next_case_code: Option<&str>,
    ) -> String {
        if !next_case_is_default {
            return current_case_code.to_string();
        }
        let Some(next_code) = next_case_code else {
            return current_case_code.to_string();
        };
        let Some(next_break) = extract_single_labeled_break_target(next_code) else {
            return current_case_code.to_string();
        };

        let trimmed = current_case_code.trim_end();
        let Some(last_newline) = trimmed.rfind('\n') else {
            return current_case_code.to_string();
        };
        let last_line = trimmed[last_newline + 1..].trim();
        if last_line == next_break {
            let mut out = trimmed[..last_newline].to_string();
            if !out.is_empty() {
                out.push('\n');
            }
            out
        } else {
            current_case_code.to_string()
        }
    }

    fn has_explicit_break_terminator(block: &[ReactiveStatement]) -> bool {
        match block.last() {
            Some(ReactiveStatement::Terminal(term_stmt)) => {
                matches!(
                    term_stmt.terminal,
                    ReactiveTerminal::Break {
                        target_kind: ReactiveTerminalTargetKind::Unlabeled
                            | ReactiveTerminalTargetKind::Labeled,
                        ..
                    }
                )
            }
            _ => false,
        }
    }

    fn extract_direct_store_assignment(
        stmt: &ReactiveStatement,
    ) -> Option<(DeclarationId, DeclarationId)> {
        let ReactiveStatement::Instruction(instr) = stmt else {
            return None;
        };
        match &instr.value {
            InstructionValue::StoreLocal { lvalue, value, .. }
            | InstructionValue::StoreContext { lvalue, value, .. } => Some((
                lvalue.place.identifier.declaration_id,
                value.identifier.declaration_id,
            )),
            _ => None,
        }
    }

    fn is_zero_dep_literal_scope_decl(
        stmt: &ReactiveStatement,
        declaration_id: DeclarationId,
    ) -> bool {
        let scope_block = match stmt {
            ReactiveStatement::Scope(scope_block) => scope_block,
            _ => return false,
        };
        if !scope_block.scope.dependencies.is_empty()
            || !scope_block.scope.reassignments.is_empty()
            || scope_block.scope.declarations.len() != 1
            || scope_block.instructions.len() != 1
        {
            return false;
        }
        let Some(scope_decl) = scope_block.scope.declarations.values().next() else {
            return false;
        };
        if scope_decl.identifier.declaration_id != declaration_id {
            return false;
        }
        let ReactiveStatement::Instruction(instr) = &scope_block.instructions[0] else {
            return false;
        };
        let Some(lvalue) = &instr.lvalue else {
            return false;
        };
        if lvalue.identifier.declaration_id != declaration_id {
            return false;
        }
        matches!(
            instr.value,
            InstructionValue::Primitive { .. }
                | InstructionValue::ArrayExpression { .. }
                | InstructionValue::ObjectExpression { .. }
        )
    }

    fn is_simple_fallthrough_reassign_block(
        block: &[ReactiveStatement],
        target_decl: DeclarationId,
    ) -> bool {
        if block.is_empty() {
            return false;
        }
        let mut saw_store = false;
        for stmt in block {
            if let Some((store_target, _)) = extract_direct_store_assignment(stmt)
                && store_target == target_decl
            {
                saw_store = true;
                continue;
            }

            if !saw_store {
                let ReactiveStatement::Instruction(instr) = stmt else {
                    return false;
                };
                if reactive_instruction_uses_declaration(instr, target_decl)
                    || reactive_instruction_writes_declaration(instr, target_decl)
                    || !is_fusable_inline_temp_instruction(instr)
                {
                    return false;
                }
                continue;
            }

            if !matches!(
                stmt,
                ReactiveStatement::Terminal(ReactiveTerminalStatement {
                    terminal: ReactiveTerminal::Break { .. },
                    ..
                })
            ) {
                return false;
            }
        }
        saw_store
    }

    fn has_direct_store_assignment(block: &[ReactiveStatement]) -> bool {
        block
            .iter()
            .any(|stmt| extract_direct_store_assignment(stmt).is_some())
    }

    fn maybe_trim_dead_switch_case_tail_for_fallthrough(
        previous_case_block: Option<&[ReactiveStatement]>,
        current_case_block: &[ReactiveStatement],
        next_case_block: Option<&[ReactiveStatement]>,
    ) -> Option<usize> {
        let next_case_block = next_case_block?;
        if current_case_block.len() < 3 || !has_explicit_break_terminator(current_case_block) {
            return None;
        }

        // Pattern 0: case value is overwritten by the next case/default before any
        // externally visible use. Keep this case empty to preserve fallthrough shape.
        let current_store_idx = current_case_block.len() - 2;
        let next_store_idx = next_case_block.len().checked_sub(2)?;
        if let Some((current_target, _)) =
            extract_direct_store_assignment(&current_case_block[current_store_idx])
            && let Some((next_target, _)) =
                extract_direct_store_assignment(&next_case_block[next_store_idx])
            && previous_case_block.is_some_and(|prev| !has_direct_store_assignment(prev))
            && current_target == next_target
            && current_case_block[current_store_idx + 1..]
                .iter()
                .all(|stmt| {
                    matches!(
                        stmt,
                        ReactiveStatement::Terminal(ReactiveTerminalStatement {
                            terminal: ReactiveTerminal::Break { .. },
                            ..
                        })
                    )
                })
        {
            let mut prefix_is_pure = true;
            for stmt in &current_case_block[..current_store_idx] {
                let ReactiveStatement::Instruction(instr) = stmt else {
                    prefix_is_pure = false;
                    break;
                };
                if reactive_instruction_uses_declaration(instr, current_target)
                    || reactive_instruction_writes_declaration(instr, current_target)
                    || !is_fusable_inline_temp_instruction(instr)
                {
                    prefix_is_pure = false;
                    break;
                }
            }
            if prefix_is_pure {
                debug_codegen_expr(
                    "switch-fallthrough-dead-store-trim",
                    format!(
                        "drop_case_body current_len={} next_len={} target_decl={}",
                        current_case_block.len(),
                        next_case_block.len(),
                        current_target.0
                    ),
                );
                return Some(0);
            }
        }

        let store_idx = current_case_block.len() - 2;
        let scope_idx = current_case_block.len() - 3;
        let (target_decl, source_decl) =
            extract_direct_store_assignment(&current_case_block[store_idx])?;

        if target_decl == source_decl
            || !is_zero_dep_literal_scope_decl(&current_case_block[scope_idx], source_decl)
            || !is_simple_fallthrough_reassign_block(next_case_block, target_decl)
            || reactive_block_uses_declaration(&current_case_block[..scope_idx], source_decl)
            || reactive_block_uses_declaration(next_case_block, source_decl)
        {
            return None;
        }

        Some(scope_idx)
    }

    match terminal {
        ReactiveTerminal::Break {
            target,
            target_kind,
            ..
        } => match target_kind {
            ReactiveTerminalTargetKind::Implicit => None,
            ReactiveTerminalTargetKind::Unlabeled => render_reactive_break_statement_ast(None),
            ReactiveTerminalTargetKind::Labeled => {
                render_reactive_break_statement_ast(Some(&format!("bb{}", target.0)))
            }
        },
        ReactiveTerminal::Continue {
            target,
            target_kind,
            ..
        } => match target_kind {
            ReactiveTerminalTargetKind::Implicit => None,
            ReactiveTerminalTargetKind::Unlabeled => render_reactive_continue_statement_ast(None),
            ReactiveTerminalTargetKind::Labeled => {
                render_reactive_continue_statement_ast(Some(&format!("bb{}", target.0)))
            }
        },
        ReactiveTerminal::Return { value, .. } => {
            let expr = codegen_place_to_expression(cx, value);
            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                eprintln!(
                    "[CODEGEN_RETURN] id={} decl={} name={:?} expr={}",
                    value.identifier.id.0,
                    value.identifier.declaration_id.0,
                    value.identifier.name,
                    expr
                );
            }
            if expr == "undefined" {
                render_reactive_return_statement_ast(None)
            } else {
                render_reactive_return_statement_ast(Some(&expr))
            }
        }
        ReactiveTerminal::Throw { value, .. } => {
            let expr = codegen_place_to_expression(cx, value);
            render_reactive_throw_statement_ast(&expr)
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            let test_expr = codegen_place_to_expression(cx, test);
            let cons_block = codegen_block(cx, consequent);
            let alt_block = alternate.as_ref().map(|alt| codegen_block(cx, alt));
            if let Some(rendered) =
                render_reactive_if_statement_ast(&test_expr, &cons_block, alt_block.as_deref())
            {
                Some(rendered)
            } else {
                let mut result = format!("if ({}) {{\n{}}}", test_expr, cons_block);
                if let Some(alt_block) = alt_block
                    && !alt_block.trim().is_empty()
                {
                    result.push_str(&format!(" else {{\n{}}}", alt_block));
                }
                result.push('\n');
                Some(result)
            }
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            let test_expr = codegen_place_to_expression(cx, test);
            let block_codes: Vec<Option<String>> = cases
                .iter()
                .enumerate()
                .map(|(index, case)| {
                    case.block.as_ref().map(|block| {
                        let cleaned_prefix_len = maybe_trim_dead_switch_case_tail_for_fallthrough(
                            index
                                .checked_sub(1)
                                .and_then(|prev| cases.get(prev))
                                .and_then(|prev| prev.block.as_deref()),
                            block,
                            cases.get(index + 1).and_then(|next| next.block.as_deref()),
                        );
                        if let Some(prefix_len) = cleaned_prefix_len {
                            debug_codegen_expr(
                                "switch-fallthrough-tail-trim",
                                format!(
                                    "case_index={} dropped={} total={}",
                                    index,
                                    block.len().saturating_sub(prefix_len),
                                    block.len()
                                ),
                            );
                            codegen_block(cx, &block[..prefix_len])
                        } else {
                            codegen_block(cx, block)
                        }
                    })
                })
                .collect();
            let mut rendered_cases: Vec<(Option<String>, Option<String>)> =
                Vec::with_capacity(cases.len());
            for (index, case) in cases.iter().enumerate() {
                // Pre-compute block code to decide formatting and allow simple
                // fallthrough cleanup against the following default case.
                let mut block_code = block_codes[index].clone();
                if let Some(code) = &block_code
                    && let Some(next_case) = cases.get(index + 1)
                {
                    block_code = Some(trim_trailing_labeled_break_if_matches_default(
                        code,
                        next_case.test.is_none(),
                        block_codes.get(index + 1).and_then(|c| c.as_deref()),
                    ));
                }
                rendered_cases.push((
                    case.test
                        .as_ref()
                        .map(|test| codegen_place_to_expression(cx, test)),
                    block_code.filter(|code| !code.trim().is_empty()),
                ));
            }
            if let Some(rendered) =
                render_reactive_switch_statement_ast(&test_expr, &rendered_cases)
            {
                Some(rendered)
            } else {
                let mut result = format!("switch ({}) {{\n", test_expr);
                for (case_test, block_code) in rendered_cases {
                    let has_block = block_code
                        .as_ref()
                        .is_some_and(|code| !code.trim().is_empty());

                    if let Some(test_expr) = case_test {
                        if has_block {
                            result.push_str(&format!("case {}: ", test_expr));
                        } else {
                            result.push_str(&format!("case {}:\n", test_expr));
                        }
                    } else if has_block {
                        result.push_str("default: ");
                    } else {
                        result.push_str("default:\n");
                    }
                    if let Some(block_code) = block_code
                        && !block_code.trim().is_empty()
                    {
                        result.push_str(&format!("{{\n{}}}\n", block_code));
                    }
                }
                result.push_str("}\n");
                Some(result)
            }
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            loop_block,
            loc,
            ..
        } => {
            let mut init_code = codegen_for_init(cx, init);
            let test_expr = codegen_place_to_expression(cx, test);
            let update_code = if let Some(upd) = update {
                codegen_for_update(cx, upd)
            } else {
                String::new()
            };
            if let Some(filled_init) =
                maybe_fill_for_header_initializer_from_update(&init_code, &update_code)
            {
                init_code = filled_init;
            }
            let body = codegen_block(cx, loop_block);
            let single_line_header = format!("for ({}; {}; {})", init_code, test_expr, update_code);
            let multiline_header = single_line_header.len() > 80
                || init_code.contains('\n')
                || test_expr.contains('\n')
                || update_code.contains('\n');
            let _ = loc;
            if let Some(rendered) =
                render_reactive_for_statement_ast(&init_code, &test_expr, &update_code, &body)
            {
                Some(rendered)
            } else if multiline_header {
                Some(format!(
                    "for (\n{};\n{};\n{}) {{\n{}}}\n",
                    init_code, test_expr, update_code, body
                ))
            } else {
                Some(format!(
                    "for ({}; {}; {}) {{\n{}}}\n",
                    init_code, test_expr, update_code, body
                ))
            }
        }
        ReactiveTerminal::ForOf {
            init,
            test,
            loop_block,
            ..
        } => {
            // Keep init/test-derived temporaries available for collection/lvalue
            // reconstruction in the for-of header.
            let init_out = codegen_block_no_reset(cx, init);
            let (kind, lval, collection_place) = extract_for_in_of_header_from_init(cx, init);
            let collection_expr = if let Some(place) = collection_place {
                codegen_place_to_expression(cx, &place)
            } else if test.identifier.id.0 == 0 {
                let s = init_out.trim().trim_end_matches(';').to_string();
                if s.is_empty() {
                    codegen_place_to_expression(cx, test)
                } else {
                    s
                }
            } else {
                codegen_place_to_expression(cx, test)
            };
            let body = codegen_block(cx, loop_block);
            render_reactive_for_of_statement_ast(&kind, &lval, &collection_expr, &body).or_else(
                || {
                    Some(format!(
                        "for ({} {} of {}) {{\n{}}}\n",
                        kind, lval, collection_expr, body
                    ))
                },
            )
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            let init_out = codegen_block_no_reset(cx, init);
            let (kind, lval, collection_place) = extract_for_in_of_header_from_init(cx, init);
            let collection = if let Some(place) = collection_place {
                codegen_place_to_expression(cx, &place)
            } else {
                init_out.trim().trim_end_matches(';').to_string()
            };
            let body = codegen_block(cx, loop_block);
            render_reactive_for_in_statement_ast(&kind, &lval, &collection, &body).or_else(|| {
                Some(format!(
                    "for ({} {} in {}) {{\n{}}}\n",
                    kind, lval, collection, body
                ))
            })
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            let test_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
            let body = codegen_block(cx, loop_block);
            render_reactive_while_statement_ast(&test_expr, &body)
                .or_else(|| Some(format!("while ({}) {{\n{}}}\n", test_expr, body)))
        }
        ReactiveTerminal::DoWhile {
            test, loop_block, ..
        } => {
            if block_has_unconditional_break_terminator(loop_block) {
                let mut one_pass = String::new();
                for stmt in loop_block {
                    match stmt {
                        ReactiveStatement::Instruction(instr) => {
                            if let Some(code) = codegen_instruction_nullable(cx, instr) {
                                one_pass.push_str(&code);
                                if !code.ends_with('\n') {
                                    one_pass.push('\n');
                                }
                            }
                        }
                        ReactiveStatement::Terminal(term_stmt)
                            if matches!(term_stmt.terminal, ReactiveTerminal::Break { .. }) =>
                        {
                            break;
                        }
                        _ => return None,
                    }
                }
                Some(one_pass)
            } else {
                let test_expr = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
                let body = codegen_block(cx, loop_block);
                render_reactive_do_while_statement_ast(&body, &test_expr)
                    .or_else(|| Some(format!("do {{\n{}}} while ({});\n", body, test_expr)))
            }
        }
        ReactiveTerminal::Label { block, .. } => Some(codegen_block(cx, block)),
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            let try_body = codegen_block(cx, block);
            let (catch_param, catch_alias) = if let Some(binding) = handler_binding {
                cx.set_temp_expr(&binding.identifier, None);
                let original_name = identifier_name_with_cx(cx, &binding.identifier);
                // Upstream promotes catch bindings to temporaries (e.g., e → t1).
                // If the binding has a user-visible (non-temp) name, generate a temp
                // name for the catch parameter and add a `const e = t1;` alias if
                // the original name is referenced in the catch body.
                if !is_codegen_temp_name(&original_name) {
                    let temp_name = fresh_temp_name(cx);
                    // Check if the original name is referenced in the handler block
                    let original_used = handler_block_references_name(handler, &original_name);
                    let alias = if original_used {
                        render_reactive_variable_statement_ast(
                            ast::VariableDeclarationKind::Const,
                            &original_name,
                            Some(&temp_name),
                        )
                    } else {
                        None
                    };
                    (temp_name, alias)
                } else {
                    (original_name, None)
                }
            } else {
                (String::new(), None)
            };
            let catch_body = codegen_block(cx, handler);
            let rendered_catch_body = if let Some(alias) = &catch_alias {
                format!("{alias}{catch_body}")
            } else {
                catch_body.clone()
            };
            if let Some(rendered) = render_reactive_try_statement_ast(
                &try_body,
                if catch_param.is_empty() {
                    None
                } else {
                    Some(catch_param.as_str())
                },
                Some(&rendered_catch_body),
            ) {
                Some(rendered)
            } else if catch_param.is_empty() {
                Some(format!(
                    "try {{\n{}}} catch {{\n{}}}\n",
                    try_body, catch_body
                ))
            } else {
                let full_catch_body = if let Some(alias) = catch_alias {
                    format!("{}{}", alias, catch_body)
                } else {
                    catch_body
                };
                Some(format!(
                    "try {{\n{}}} catch ({}) {{\n{}}}\n",
                    try_body, catch_param, full_catch_body
                ))
            }
        }
    }
}

/// Generate code for an instruction, returning None if no statement is needed.
fn codegen_instruction_nullable(cx: &mut Context, instr: &ReactiveInstruction) -> Option<String> {
    // Track stable setter declarations (setState/dispatch-like values).
    if let Some(lvalue) = &instr.lvalue
        && is_stable_setter_identifier(&lvalue.identifier)
    {
        cx.mark_stable_setter_identifier(&lvalue.identifier);
    }

    // Track resolved names through lowered aliases for better hook matching.
    if let Some(lvalue) = &instr.lvalue {
        match &instr.value {
            InstructionValue::CallExpression { callee, .. } => {
                if let Some(name) = resolve_place_name(cx, callee)
                    && let Some(hook) = extract_hook_name(&name)
                {
                    cx.hook_call_by_decl
                        .insert(lvalue.identifier.declaration_id, hook.to_string());
                    if hook == "useEffectEvent" {
                        cx.stable_effect_event_decls
                            .insert(lvalue.identifier.declaration_id);
                    }
                }
            }
            InstructionValue::MethodCall { property, .. } => {
                if let Some(name) = resolve_place_name(cx, property)
                    && let Some(hook) = extract_hook_name(&name)
                {
                    cx.hook_call_by_decl
                        .insert(lvalue.identifier.declaration_id, hook.to_string());
                    if hook == "useEffectEvent" {
                        cx.stable_effect_event_decls
                            .insert(lvalue.identifier.declaration_id);
                    }
                }
            }
            InstructionValue::LoadGlobal { binding, .. } => {
                cx.resolved_names
                    .insert(lvalue.identifier.id, load_global_resolved_name(binding));
                cx.non_local_binding_decls
                    .insert(lvalue.identifier.declaration_id);
            }
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                if let Some(name) = resolve_place_name(cx, place) {
                    cx.resolved_names.insert(lvalue.identifier.id, name);
                }
                if cx
                    .multi_source_decls
                    .contains(&place.identifier.declaration_id)
                {
                    cx.multi_source_decls
                        .insert(lvalue.identifier.declaration_id);
                }
                if cx
                    .non_local_binding_decls
                    .contains(&place.identifier.declaration_id)
                {
                    cx.non_local_binding_decls
                        .insert(lvalue.identifier.declaration_id);
                }
                if let Some(hook) = cx
                    .hook_call_by_decl
                    .get(&place.identifier.declaration_id)
                    .cloned()
                {
                    cx.hook_call_by_decl
                        .insert(lvalue.identifier.declaration_id, hook);
                    if cx
                        .stable_effect_event_decls
                        .contains(&place.identifier.declaration_id)
                    {
                        cx.stable_effect_event_decls
                            .insert(lvalue.identifier.declaration_id);
                    }
                }
            }
            InstructionValue::TypeCastExpression { value, .. } => {
                if let Some(name) = resolve_place_name(cx, value) {
                    cx.resolved_names.insert(lvalue.identifier.id, name);
                }
                if cx
                    .multi_source_decls
                    .contains(&value.identifier.declaration_id)
                {
                    cx.multi_source_decls
                        .insert(lvalue.identifier.declaration_id);
                }
                if cx
                    .non_local_binding_decls
                    .contains(&value.identifier.declaration_id)
                {
                    cx.non_local_binding_decls
                        .insert(lvalue.identifier.declaration_id);
                }
                if let Some(hook) = cx
                    .hook_call_by_decl
                    .get(&value.identifier.declaration_id)
                    .cloned()
                {
                    cx.hook_call_by_decl
                        .insert(lvalue.identifier.declaration_id, hook);
                    if cx
                        .stable_effect_event_decls
                        .contains(&value.identifier.declaration_id)
                    {
                        cx.stable_effect_event_decls
                            .insert(lvalue.identifier.declaration_id);
                    }
                }
            }
            InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            } => {
                cx.resolved_names.insert(lvalue.identifier.id, s.clone());
            }
            InstructionValue::PropertyLoad {
                object,
                property,
                optional,
                ..
            } => {
                if let Some(object_name) = resolve_place_name(cx, object) {
                    cx.resolved_names.insert(
                        lvalue.identifier.id,
                        format_property_access(&object_name, property, *optional),
                    );
                } else if let PropertyLiteral::String(prop) = property {
                    cx.resolved_names.insert(lvalue.identifier.id, prop.clone());
                }
            }
            _ => {}
        }
    }

    // Mark stable destructured hook tuple positions (e.g. useState setter).
    if let InstructionValue::Destructure { lvalue, value, .. } = &instr.value
        && is_stable_setter_hook_result_type(&value.identifier)
        && let Some(hook_name) = cx
            .hook_call_by_decl
            .get(&value.identifier.declaration_id)
            .cloned()
        && let Pattern::Array(arr) = &lvalue.pattern
    {
        for (idx, elem) in arr.items.iter().enumerate() {
            if let ArrayElement::Place(place) = elem
                && is_stable_setter_hook_element(&hook_name, idx)
            {
                cx.mark_stable_setter_identifier(&place.identifier);
            }
        }
    }

    // Track stable `useRef` values (and aliases) so infer-effect-deps can
    // avoid over-capturing `.current` usage for stable local refs.
    if let Some(lvalue) = &instr.lvalue {
        let stable_from_rhs = match &instr.value {
            InstructionValue::CallExpression { callee, .. } => {
                resolve_place_name(cx, callee).is_some_and(|name| is_use_ref_name(&name))
            }
            InstructionValue::MethodCall { property, .. } => {
                resolve_place_name(cx, property).is_some_and(|name| is_use_ref_name(&name))
            }
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => cx
                .stable_ref_decls
                .contains(&place.identifier.declaration_id),
            InstructionValue::TypeCastExpression { value, .. } => cx
                .stable_ref_decls
                .contains(&value.identifier.declaration_id),
            InstructionValue::Ternary {
                consequent,
                alternate,
                ..
            } => {
                cx.stable_ref_decls
                    .contains(&consequent.identifier.declaration_id)
                    && cx
                        .stable_ref_decls
                        .contains(&alternate.identifier.declaration_id)
            }
            InstructionValue::LogicalExpression { left, right, .. } => {
                cx.stable_ref_decls
                    .contains(&left.identifier.declaration_id)
                    && cx
                        .stable_ref_decls
                        .contains(&right.identifier.declaration_id)
            }
            _ => false,
        };
        if stable_from_rhs {
            cx.mark_stable_ref_identifier(&lvalue.identifier);
        }
    }

    // Track callback dependency metadata across aliases so later calls with
    // AUTODEPS can materialize concrete dependency arrays.
    if let Some(lvalue) = &instr.lvalue {
        match &instr.value {
            InstructionValue::FunctionExpression { lowered_func, .. }
            | InstructionValue::ObjectMethod { lowered_func, .. } => {
                let decl_id = lvalue.identifier.declaration_id;
                let has_existing_non_empty = cx
                    .callback_deps
                    .get(&decl_id)
                    .is_some_and(|deps| !deps.is_empty());
                if !has_existing_non_empty {
                    let primitive_literals_for_child = cx.primitive_literals_for_child();
                    let deps = infer_callback_dependency_paths(
                        lowered_func,
                        &cx.stable_ref_decls,
                        &cx.stable_setter_decls,
                        &cx.stable_effect_event_decls,
                        &cx.multi_source_decls,
                        &primitive_literals_for_child,
                    );
                    cx.callback_deps.insert(decl_id, deps);
                } else if std::env::var("DEBUG_AUTODEPS_FLOW").is_ok() {
                    eprintln!(
                        "[AUTODEPS_FLOW] preserve callback deps for decl={} (existing scope override)",
                        decl_id.0
                    );
                }
            }
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => {
                if cx
                    .stable_zero_dep_decls
                    .contains(&place.identifier.declaration_id)
                {
                    cx.stable_zero_dep_decls
                        .insert(lvalue.identifier.declaration_id);
                } else {
                    cx.stable_zero_dep_decls
                        .remove(&lvalue.identifier.declaration_id);
                }
                if let Some(hook) = cx
                    .hook_call_by_decl
                    .get(&place.identifier.declaration_id)
                    .cloned()
                {
                    cx.hook_call_by_decl
                        .insert(lvalue.identifier.declaration_id, hook);
                }
                if let Some(deps) = cx.callback_deps.get(&place.identifier.declaration_id) {
                    cx.callback_deps
                        .insert(lvalue.identifier.declaration_id, deps.clone());
                }
            }
            InstructionValue::TypeCastExpression { value, .. } => {
                if cx
                    .stable_zero_dep_decls
                    .contains(&value.identifier.declaration_id)
                {
                    cx.stable_zero_dep_decls
                        .insert(lvalue.identifier.declaration_id);
                } else {
                    cx.stable_zero_dep_decls
                        .remove(&lvalue.identifier.declaration_id);
                }
                if let Some(deps) = cx.callback_deps.get(&value.identifier.declaration_id) {
                    cx.callback_deps
                        .insert(lvalue.identifier.declaration_id, deps.clone());
                }
            }
            _ => {}
        }
    }

    if let Some(stmt) = maybe_codegen_inline_hook_callback_with_autodeps(cx, instr) {
        return Some(stmt);
    }

    match &instr.value {
        // Store/declare instructions
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            let decl_id = lvalue.place.identifier.declaration_id;
            if lvalue.kind == InstructionKind::Reassign {
                let sources = cx.decl_assignment_sources.entry(decl_id).or_default();
                sources.insert(value.identifier.declaration_id);
                if sources.len() > 1 {
                    cx.multi_source_decls.insert(decl_id);
                }
            }
            if cx
                .multi_source_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.multi_source_decls.insert(decl_id);
            }
            if cx
                .stable_ref_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.mark_stable_ref_identifier(&lvalue.place.identifier);
            }
            if cx
                .stable_setter_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.mark_stable_setter_identifier(&lvalue.place.identifier);
            }
            if cx
                .stable_effect_event_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.stable_effect_event_decls.insert(decl_id);
            }
            if let Some(hook) = cx
                .hook_call_by_decl
                .get(&value.identifier.declaration_id)
                .cloned()
            {
                cx.hook_call_by_decl.insert(decl_id, hook);
            }
            if let Some(deps) = cx.callback_deps.get(&value.identifier.declaration_id) {
                cx.callback_deps.insert(decl_id, deps.clone());
            }
            if cx
                .stable_zero_dep_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.stable_zero_dep_decls.insert(decl_id);
            } else if lvalue.kind == InstructionKind::Reassign {
                cx.stable_zero_dep_decls.remove(&decl_id);
            }

            if lvalue.kind == InstructionKind::Reassign {
                cx.capturable_primitive_literals.remove(&decl_id);
                cx.inline_primitive_literals.remove(&decl_id);
            } else if let Some(literal) = extract_inlineable_primitive_from_place(cx, value) {
                let can_inline = !cx.reassigned_decls.contains(&decl_id)
                    && cx.captured_in_child_functions.contains(&decl_id)
                    && !cx.mutable_captured_in_child_functions.contains(&decl_id);
                if can_inline {
                    cx.capturable_primitive_literals
                        .insert(decl_id, literal.clone());
                    cx.inline_primitive_literals.insert(decl_id, literal);
                } else {
                    cx.capturable_primitive_literals.remove(&decl_id);
                    cx.inline_primitive_literals.remove(&decl_id);
                }
            } else {
                cx.capturable_primitive_literals.remove(&decl_id);
                cx.inline_primitive_literals.remove(&decl_id);
            }
            let kind = if has_materialized_named_binding(cx, &lvalue.place.identifier) {
                InstructionKind::Reassign
            } else {
                lvalue.kind
            };
            let rhs = codegen_place_to_expression(cx, value);
            if rhs == "undefined"
                && kind == InstructionKind::Reassign
                && matches!(value.identifier.loc, SourceLocation::Generated)
            {
                let is_temp_reassign =
                    lvalue
                        .place
                        .identifier
                        .name
                        .as_ref()
                        .is_some_and(|name| match name {
                            IdentifierName::Named(n) | IdentifierName::Promoted(n) => {
                                is_codegen_temp_name(n)
                            }
                        });
                if is_temp_reassign {
                    // Preserve generated temp reassigns to `undefined` for
                    // control-flow lowered value-blocks (upstream keeps these).
                } else {
                    // Lowering sometimes introduces a synthetic `x = undefined` statement
                    // for previously-declared vars. Upstream omits this write.
                    return None;
                }
            }
            if can_inline_generated_undefined_alias(cx, lvalue, value, &rhs) {
                cx.inline_identifier_aliases.insert(decl_id, rhs);
                cx.elided_named_declarations.remove(&decl_id);
                return None;
            }
            if can_inline_jsx_component_tag_alias(
                cx,
                &lvalue.place.identifier,
                value.identifier.declaration_id,
                &rhs,
            ) {
                cx.inline_identifier_aliases.insert(decl_id, rhs);
                cx.declare(&lvalue.place.identifier);
                cx.elided_named_declarations.insert(decl_id);
                return None;
            }

            let mut emitted = String::new();
            if let Some(prefix) =
                maybe_materialize_elided_named_declaration(cx, &lvalue.place.identifier)
            {
                emitted.push_str(&prefix);
            }
            cx.inline_identifier_aliases.remove(&decl_id);
            if let Some(stmt) = codegen_store(cx, instr, kind, &lvalue.place, &rhs) {
                emitted.push_str(&stmt);
            }
            if emitted.is_empty() {
                None
            } else {
                Some(emitted)
            }
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            let decl_id = lvalue.place.identifier.declaration_id;
            if lvalue.kind == InstructionKind::Reassign {
                let sources = cx.decl_assignment_sources.entry(decl_id).or_default();
                sources.insert(value.identifier.declaration_id);
                if sources.len() > 1 {
                    cx.multi_source_decls.insert(decl_id);
                }
            }
            if cx
                .multi_source_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.multi_source_decls.insert(decl_id);
            }
            if cx
                .stable_ref_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.mark_stable_ref_identifier(&lvalue.place.identifier);
            }
            if cx
                .stable_setter_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.mark_stable_setter_identifier(&lvalue.place.identifier);
            }
            if cx
                .stable_effect_event_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.stable_effect_event_decls.insert(decl_id);
            }
            if let Some(hook) = cx
                .hook_call_by_decl
                .get(&value.identifier.declaration_id)
                .cloned()
            {
                cx.hook_call_by_decl.insert(decl_id, hook);
            }
            if let Some(deps) = cx.callback_deps.get(&value.identifier.declaration_id) {
                cx.callback_deps.insert(decl_id, deps.clone());
            }
            if cx
                .stable_zero_dep_decls
                .contains(&value.identifier.declaration_id)
            {
                cx.stable_zero_dep_decls.insert(decl_id);
            } else if lvalue.kind == InstructionKind::Reassign {
                cx.stable_zero_dep_decls.remove(&decl_id);
            }

            if lvalue.kind == InstructionKind::Reassign {
                cx.capturable_primitive_literals.remove(&decl_id);
                cx.inline_primitive_literals.remove(&decl_id);
            } else if let Some(literal) = extract_inlineable_primitive_from_place(cx, value) {
                let can_inline = !cx.reassigned_decls.contains(&decl_id)
                    && cx.captured_in_child_functions.contains(&decl_id)
                    && !cx.mutable_captured_in_child_functions.contains(&decl_id);
                if can_inline {
                    cx.capturable_primitive_literals
                        .insert(decl_id, literal.clone());
                    cx.inline_primitive_literals.insert(decl_id, literal);
                } else {
                    cx.capturable_primitive_literals.remove(&decl_id);
                    cx.inline_primitive_literals.remove(&decl_id);
                }
            } else {
                cx.capturable_primitive_literals.remove(&decl_id);
                cx.inline_primitive_literals.remove(&decl_id);
            }
            let rhs = codegen_place_to_expression(cx, value);
            if rhs == "undefined"
                && lvalue.kind == InstructionKind::Reassign
                && matches!(value.identifier.loc, SourceLocation::Generated)
            {
                let is_temp_reassign =
                    lvalue
                        .place
                        .identifier
                        .name
                        .as_ref()
                        .is_some_and(|name| match name {
                            IdentifierName::Named(n) | IdentifierName::Promoted(n) => {
                                is_codegen_temp_name(n)
                            }
                        });
                if !is_temp_reassign {
                    // Upstream omits synthetic `x = undefined` reassignments
                    // introduced by lowering when the variable is already declared.
                    return None;
                }
            }
            if instr.lvalue.is_some() && lvalue.kind == InstructionKind::Reassign {
                // Upstream codegen treats StoreContext reassignment expressions with an
                // instruction lvalue as full statements, not temp-only aliases.
                // This preserves source ordering when assignment results are consumed
                // later across interposing side effects.
                let lhs = identifier_name_with_cx(cx, &lvalue.place.identifier);
                let expr = format!("{lhs} = {rhs}");
                return codegen_instruction_expr_with_prec_kind(
                    cx,
                    instr,
                    &expr,
                    ExprPrecedence::Assignment,
                    ExprKind::Normal,
                );
            }
            if can_inline_generated_undefined_alias(cx, lvalue, value, &rhs) {
                cx.inline_identifier_aliases.insert(decl_id, rhs);
                cx.elided_named_declarations.remove(&decl_id);
                return None;
            }
            let mut emitted = String::new();
            if let Some(prefix) =
                maybe_materialize_elided_named_declaration(cx, &lvalue.place.identifier)
            {
                emitted.push_str(&prefix);
            }
            cx.inline_identifier_aliases.remove(&decl_id);
            if let Some(stmt) = codegen_store(cx, instr, lvalue.kind, &lvalue.place, &rhs) {
                emitted.push_str(&stmt);
            }
            if emitted.is_empty() {
                None
            } else {
                Some(emitted)
            }
        }
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            if instr.lvalue.is_some() {
                match lvalue.kind {
                    InstructionKind::Const
                    | InstructionKind::HoistedConst
                    | InstructionKind::Let
                    | InstructionKind::HoistedLet => {
                        set_const_declaration_expression_error(cx, lvalue.kind);
                    }
                    InstructionKind::Function | InstructionKind::HoistedFunction => {
                        set_function_declaration_expression_error(cx, lvalue.kind);
                    }
                    _ => {}
                }
            }
            // DeclareLocal with Catch kind produces no output (matches upstream
            // CodegenReactiveFunction behavior: catch bindings are declared
            // via the catch clause parameter, not via DeclareLocal).
            if lvalue.kind == InstructionKind::Catch {
                return None;
            }
            if cx.has_declared(&lvalue.place.identifier) {
                return None;
            }
            let decl_id = lvalue.place.identifier.declaration_id;
            let raw_name = identifier_name_static(&lvalue.place.identifier);
            let shadows_existing_name = cx.used_declaration_names.contains(&raw_name)
                || raw_name.rsplit_once('_').is_some_and(|(base, suffix)| {
                    !base.is_empty()
                        && suffix.chars().all(|ch| ch.is_ascii_digit())
                        && cx.used_declaration_names.contains(base)
                });
            let can_drop_dead_declare = !cx.read_declarations.contains(&decl_id)
                && !cx.reassigned_decls.contains(&decl_id)
                && !cx.preserve_loop_header_inits
                && shadows_existing_name
                && matches!(
                    lvalue.kind,
                    InstructionKind::Const
                        | InstructionKind::HoistedConst
                        | InstructionKind::Let
                        | InstructionKind::HoistedLet
                );
            if std::env::var("DEBUG_DECLARE_DROP").is_ok() {
                let identifier_name = lvalue
                    .place
                    .identifier
                    .name
                    .as_ref()
                    .map(IdentifierName::value)
                    .unwrap_or("-");
                eprintln!(
                    "[DECLARE_DROP] decl={} ident={} kind={:?} loc={} read={} reassigned={} shadowed_name={} drop={}",
                    decl_id.0,
                    identifier_name,
                    lvalue.kind,
                    debug_loc(&lvalue.place.identifier.loc),
                    cx.read_declarations.contains(&decl_id),
                    cx.reassigned_decls.contains(&decl_id),
                    shadows_existing_name,
                    can_drop_dead_declare
                );
            }
            if can_drop_dead_declare {
                // Keep declaration tracking coherent while omitting dead runtime
                // `let` placeholders that upstream prunes.
                cx.declare(&lvalue.place.identifier);
                cx.inline_identifier_aliases.remove(&decl_id);
                cx.elided_named_declarations.remove(&decl_id);
                return None;
            }
            let name = identifier_name_with_cx(cx, &lvalue.place.identifier);
            cx.declare(&lvalue.place.identifier);
            cx.inline_identifier_aliases.remove(&decl_id);
            cx.elided_named_declarations.remove(&decl_id);
            cx.mark_decl_runtime_emitted(decl_id);
            render_reactive_declare_local_statement_ast(&name)
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            let kind = lvalue.kind;
            if instr.lvalue.is_some() {
                match kind {
                    InstructionKind::Const
                    | InstructionKind::HoistedConst
                    | InstructionKind::Let
                    | InstructionKind::HoistedLet => {
                        set_const_declaration_expression_error(cx, kind);
                    }
                    InstructionKind::Function | InstructionKind::HoistedFunction => {
                        set_function_declaration_expression_error(cx, kind);
                    }
                    _ => {}
                }
            }
            let rhs = codegen_place_to_expression(cx, value);
            let pattern = if is_stable_setter_hook_result_type(&value.identifier)
                && let (Some(hook_name), Pattern::Array(arr)) = (
                    cx.hook_call_by_decl.get(&value.identifier.declaration_id),
                    &lvalue.pattern,
                ) {
                let mut arr = arr.clone();
                for (idx, item) in arr.items.iter_mut().enumerate() {
                    if let ArrayElement::Place(place) = item {
                        let decl_id = place.identifier.declaration_id;
                        if !is_stable_setter_hook_element(hook_name, idx)
                            && !cx.read_declarations.contains(&decl_id)
                        {
                            *item = ArrayElement::Hole;
                        }
                    }
                }
                Pattern::Array(arr)
            } else {
                lvalue.pattern.clone()
            };
            let lval = codegen_pattern(cx, &pattern);
            let operands = pattern_operands(&pattern);

            // Check if all pattern operands are already declared
            let all_declared = operands.iter().all(|p| cx.has_declared(&p.identifier));
            let all_codegen_temps = !operands.is_empty()
                && operands.iter().all(|p| {
                    let name = identifier_name_static(&p.identifier);
                    is_codegen_temp_name(&name)
                });
            // Upstream consistently emits temporary-pattern destructures as
            // declarations (`let`), even when our lowered kind is `Reassign`.
            let force_temp_declare = all_codegen_temps;
            debug_codegen_expr(
                "destructure",
                format!(
                    "kind={:?} pattern={} rhs={} all_declared={} force_temp_declare={}",
                    kind, lval, rhs, all_declared, force_temp_declare
                ),
            );

            if let Some(bridged) =
                maybe_codegen_captured_context_destructure_bridge(cx, &pattern, &rhs, all_declared)
            {
                return Some(bridged);
            }

            if (all_declared || kind == InstructionKind::Reassign) && !force_temp_declare {
                render_reactive_destructure_statement_ast(cx, &pattern, &rhs, None)
            } else {
                // Declare all pattern operands
                for p in operands {
                    cx.declare(&p.identifier);
                    if force_temp_declare
                        && let Some(name) = cx
                            .declaration_name_overrides
                            .get(&p.identifier.declaration_id)
                        && let Some(names) = cx.block_scope_declared_temp_names.last_mut()
                    {
                        names.insert(name.clone());
                    }
                }
                let declaration_kind = match kind {
                    InstructionKind::Const | InstructionKind::Function => {
                        ast::VariableDeclarationKind::Const
                    }
                    _ => ast::VariableDeclarationKind::Let,
                };
                render_reactive_destructure_statement_ast(
                    cx,
                    &pattern,
                    &rhs,
                    Some(declaration_kind),
                )
            }
        }
        // No-op instructions
        InstructionValue::StartMemoize {
            manual_memo_id,
            deps,
            ..
        } => {
            let mut dep_roots: HashSet<DeclarationId> = HashSet::new();
            if let Some(deps) = deps {
                for dep in deps {
                    if let ManualMemoRoot::NamedLocal(place) = &dep.root {
                        dep_roots.insert(place.identifier.declaration_id);
                        if dep.path.is_empty() {
                            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                                eprintln!(
                                    "[START_MEMO_DEP] decl={} name={:?}",
                                    place.identifier.declaration_id.0, place.identifier.name
                                );
                            }
                            cx.pending_manual_memo_reads
                                .insert(place.identifier.declaration_id);
                        }
                    }
                }
            }
            cx.manual_memo_dep_roots_by_id
                .insert(*manual_memo_id, dep_roots);
            None
        }
        InstructionValue::FinishMemoize {
            manual_memo_id,
            decl,
            pruned,
            ..
        } => {
            if let Some(dep_roots) = cx.manual_memo_dep_roots_by_id.get(manual_memo_id).cloned() {
                cx.manual_memo_dep_roots_by_decl
                    .insert(decl.identifier.declaration_id, dep_roots);
            }
            if *pruned {
                cx.pruned_manual_memo_decls
                    .insert(decl.identifier.declaration_id);
            }
            None
        }
        InstructionValue::Debugger { .. } => Some("debugger;\n".to_string()),
        InstructionValue::ObjectMethod { lowered_func, .. } => {
            if let Some(lvalue) = &instr.lvalue {
                let info = ObjectMethodInfo {
                    lowered_func: lowered_func.clone(),
                };
                let idx = cx.object_methods_store.len();
                cx.object_methods_store.push(info);
                cx.object_methods.insert(lvalue.identifier.id, idx);
            }
            None
        }
        InstructionValue::LoadGlobal { binding: _, .. } => {
            let ev = codegen_instruction_value_ev(cx, &instr.value);
            codegen_instruction_expr_with_prec_kind(cx, instr, &ev.expr, ev.prec, ExprKind::Normal)
        }
        // All other instruction values -> expression
        _ => {
            let ev = codegen_instruction_value_ev(cx, &instr.value);
            let stmt =
                codegen_instruction_expr_with_prec_kind(cx, instr, &ev.expr, ev.prec, ev.kind);
            if let InstructionValue::FunctionExpression { lowered_func, .. }
            | InstructionValue::ObjectMethod { lowered_func, .. } = &instr.value
            {
                cx.reserved_child_decl_names.extend(
                    collect_local_declaration_names_from_lowered_function(lowered_func),
                );
            }
            stmt
        }
    }
}

fn maybe_codegen_inline_hook_callback_with_autodeps(
    cx: &mut Context,
    instr: &ReactiveInstruction,
) -> Option<String> {
    // Keep this narrowly scoped: only transform statement-position hook calls
    // that still contain an inline callback and AUTODEPS placeholder.
    if instr
        .lvalue
        .as_ref()
        .and_then(|lv| lv.identifier.name.as_ref())
        .is_some()
    {
        return None;
    }

    match &instr.value {
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => {
            let is_hook_call = resolve_place_name(cx, callee)
                .as_deref()
                .is_some_and(|name| extract_hook_name(name).is_some());
            let callee_expr = codegen_place_to_expression(cx, callee);
            let mut rendered_args: Vec<String> =
                args.iter().map(|a| codegen_argument(cx, a)).collect();
            let autodeps_index = rendered_args
                .iter()
                .position(|arg| arg == "AUTODEPS" || arg.ends_with(".AUTODEPS"))?;
            let hook_name = callee
                .identifier
                .name
                .as_ref()
                .map(|n| n.value().to_string())
                .unwrap_or_else(|| callee_expr.clone());
            let hook_is_direct = callee.identifier.name.is_some();
            maybe_replace_autodeps_with_inferred_deps(
                cx,
                &hook_name,
                args,
                &mut rendered_args,
                hook_is_direct,
            );
            let callee_trimmed = callee_expr.trim_start();
            let needs_wrap = callee_expr.contains("=>")
                || callee_trimmed.starts_with("function ")
                || callee_trimmed.starts_with("function*")
                || callee_trimmed.starts_with("async function ")
                || callee_trimmed.starts_with("async function*");
            let callee_final = if needs_wrap {
                format!("({})", callee_expr)
            } else {
                callee_expr.clone()
            };
            if cx.disable_memoization_features {
                let rendered_args_joined = join_call_arguments(&rendered_args);
                let call_expr = if *optional {
                    format!("{}?.({})", callee_final, rendered_args_joined)
                } else {
                    format!("{}({})", callee_final, rendered_args_joined)
                };
                let call_expr = if cx.emit_hook_guards && is_hook_call {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                return render_reactive_expression_statement_ast(&call_expr);
            }
            let callback_index = (0..autodeps_index).rev().find(|idx| {
                let arg = rendered_args[*idx].trim();
                arg.contains("=>") || arg.starts_with("function")
            })?;
            let mut call_args = rendered_args.clone();
            let cb_expr = call_args[callback_index].clone();
            let cb_name = fresh_temp_name(cx);
            call_args[callback_index] = cb_name.clone();
            let deps_array_index = if callback_index + 1 < autodeps_index {
                let idx = callback_index + 1;
                let arg = call_args[idx].trim();
                if arg.starts_with('[') && arg.ends_with(']') {
                    Some(idx)
                } else {
                    None
                }
            } else {
                None
            };
            let pre = if let Some(dep_idx) = deps_array_index {
                let deps_expr = rendered_args[autodeps_index].clone();
                let deps_name = fresh_temp_name(cx);
                let cb_slot = cx.alloc_cache_slot();
                let deps_slot = cx.alloc_cache_slot();
                call_args[dep_idx] = deps_name.clone();
                let call_expr = if *optional {
                    format!("{}?.({})", callee_final, call_args.join(", "))
                } else {
                    format!("{}({})", callee_final, call_args.join(", "))
                };
                let call_expr = if cx.emit_hook_guards && is_hook_call {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                render_cached_inline_hook_callback_block_ast(
                    &cb_name,
                    &cb_expr,
                    cb_slot,
                    Some((&deps_name, &deps_expr, deps_slot)),
                    &call_expr,
                )?
            } else {
                let slot = cx.alloc_cache_slot();
                let call_expr = if *optional {
                    format!("{}?.({})", callee_final, call_args.join(", "))
                } else {
                    format!("{}({})", callee_final, call_args.join(", "))
                };
                let call_expr = if cx.emit_hook_guards && is_hook_call {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                render_cached_inline_hook_callback_block_ast(
                    &cb_name, &cb_expr, slot, None, &call_expr,
                )?
            };
            Some(pre)
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional,
            call_optional,
            ..
        } => {
            let recv = codegen_member_object_expression(cx, receiver);
            let (prop, is_computed) = resolve_method_property(cx, property, &recv);
            if is_computed || !Environment::is_hook_name(&prop) {
                return None;
            }
            let mut rendered_args: Vec<String> =
                args.iter().map(|a| codegen_argument(cx, a)).collect();
            let autodeps_index = rendered_args
                .iter()
                .position(|arg| arg == "AUTODEPS" || arg.ends_with(".AUTODEPS"))?;
            maybe_replace_autodeps_with_inferred_deps(cx, &prop, args, &mut rendered_args, true);
            let dot = if *receiver_optional { "?." } else { "." };
            if cx.disable_memoization_features {
                let rendered_args_joined = join_call_arguments(&rendered_args);
                let call_expr = if *call_optional {
                    format!("{}{}{}?.({})", recv, dot, prop, rendered_args_joined)
                } else {
                    format!("{}{}{}({})", recv, dot, prop, rendered_args_joined)
                };
                let call_expr = if cx.emit_hook_guards {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                return render_reactive_expression_statement_ast(&call_expr);
            }
            let callback_index = (0..autodeps_index).rev().find(|idx| {
                let arg = rendered_args[*idx].trim();
                arg.contains("=>") || arg.starts_with("function")
            })?;
            let mut call_args = rendered_args.clone();
            let cb_expr = call_args[callback_index].clone();
            let cb_name = fresh_temp_name(cx);
            call_args[callback_index] = cb_name.clone();
            let deps_array_index = if callback_index + 1 < autodeps_index {
                let idx = callback_index + 1;
                let arg = call_args[idx].trim();
                if arg.starts_with('[') && arg.ends_with(']') {
                    Some(idx)
                } else {
                    None
                }
            } else {
                None
            };
            let pre = if let Some(dep_idx) = deps_array_index {
                let deps_expr = rendered_args[autodeps_index].clone();
                let deps_name = fresh_temp_name(cx);
                let cb_slot = cx.alloc_cache_slot();
                let deps_slot = cx.alloc_cache_slot();
                call_args[dep_idx] = deps_name.clone();
                let call_expr = if *call_optional {
                    format!("{}{}{}?.({})", recv, dot, prop, call_args.join(", "))
                } else {
                    format!("{}{}{}({})", recv, dot, prop, call_args.join(", "))
                };
                let call_expr = if cx.emit_hook_guards {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                render_cached_inline_hook_callback_block_ast(
                    &cb_name,
                    &cb_expr,
                    cb_slot,
                    Some((&deps_name, &deps_expr, deps_slot)),
                    &call_expr,
                )?
            } else {
                let slot = cx.alloc_cache_slot();
                let call_expr = if *call_optional {
                    format!("{}{}{}?.({})", recv, dot, prop, call_args.join(", "))
                } else {
                    format!("{}{}{}({})", recv, dot, prop, call_args.join(", "))
                };
                let call_expr = if cx.emit_hook_guards {
                    wrap_hook_guarded_call_expression(&call_expr)
                } else {
                    call_expr
                };
                render_cached_inline_hook_callback_block_ast(
                    &cb_name, &cb_expr, slot, None, &call_expr,
                )?
            };
            Some(pre)
        }
        _ => None,
    }
}

/// Handle store-like instructions (StoreLocal, StoreContext).
fn is_simple_identifier_expression(expr: &str) -> bool {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$')
}

fn can_inline_jsx_component_tag_alias(
    cx: &Context,
    target_identifier: &Identifier,
    source_decl: DeclarationId,
    rhs: &str,
) -> bool {
    let decl_id = target_identifier.declaration_id;
    let component_like_name = target_identifier.name.as_ref().is_some_and(|name| {
        name.value()
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
    });
    if !cx.jsx_only_component_tag_decls.contains(&decl_id) && !component_like_name {
        return false;
    }
    if cx.captured_in_child_functions.contains(&decl_id)
        || cx.mutable_captured_in_child_functions.contains(&decl_id)
    {
        return false;
    }
    if !(cx.non_local_binding_decls.contains(&source_decl)
        || cx.inline_identifier_aliases.contains_key(&source_decl))
    {
        return false;
    }
    if !is_simple_identifier_expression(rhs) {
        return false;
    }
    !cx.has_declared_by_runtime_emission(decl_id) || cx.elided_named_declarations.contains(&decl_id)
}

fn can_inline_generated_undefined_alias(
    cx: &Context,
    lvalue: &LValue,
    value: &Place,
    rhs: &str,
) -> bool {
    rhs == "undefined"
        && lvalue.kind != InstructionKind::Reassign
        && value.identifier.name.is_none()
        && cx
            .pruned_manual_memo_decls
            .contains(&value.identifier.declaration_id)
        && !cx
            .reassigned_decls
            .contains(&lvalue.place.identifier.declaration_id)
}

fn maybe_materialize_elided_named_declaration(cx: &mut Context, id: &Identifier) -> Option<String> {
    if !cx.elided_named_declarations.contains(&id.declaration_id) {
        return None;
    }
    let name = identifier_name_with_cx(cx, id);
    let init = cx
        .inline_identifier_aliases
        .get(&id.declaration_id)
        .cloned();
    cx.inline_identifier_aliases.remove(&id.declaration_id);
    cx.elided_named_declarations.remove(&id.declaration_id);
    cx.mark_decl_runtime_emitted(id.declaration_id);
    if let Some(init_expr) = init {
        render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            &name,
            Some(&init_expr),
        )
    } else {
        render_reactive_variable_statement_ast(ast::VariableDeclarationKind::Let, &name, None)
    }
}

fn has_materialized_named_binding(cx: &Context, id: &Identifier) -> bool {
    cx.has_declared_by_runtime_emission(id.declaration_id)
        || cx.elided_named_declarations.contains(&id.declaration_id)
}

fn codegen_store(
    cx: &mut Context,
    instr: &ReactiveInstruction,
    kind: InstructionKind,
    place: &Place,
    rhs: &str,
) -> Option<String> {
    if instr.lvalue.is_some() {
        match kind {
            InstructionKind::Const | InstructionKind::Let | InstructionKind::HoistedLet => {
                set_const_declaration_expression_error(cx, kind);
            }
            InstructionKind::Function => {
                set_function_declaration_expression_error(cx, kind);
            }
            _ => {}
        }
    }

    let name = identifier_name_with_cx(cx, &place.identifier);
    if std::env::var("DEBUG_CODEGEN_STORE").is_ok() {
        eprintln!(
            "[CODEGEN_STORE] kind={:?} decl={} name={} has_outer_lvalue={} rhs={}",
            kind,
            place.identifier.declaration_id.0,
            name,
            instr.lvalue.is_some(),
            rhs
        );
    }
    match kind {
        InstructionKind::Const | InstructionKind::Function => {
            let decl_id = place.identifier.declaration_id;
            let can_drop_unused_const_literal = kind == InstructionKind::Const
                && instr.lvalue.is_none()
                && !cx.read_declarations.contains(&decl_id)
                && !cx.manual_memo_root_decls.contains(&decl_id)
                && !cx.function_decl_decls.contains(&decl_id)
                && !cx.preserve_loop_header_inits
                && is_inlineable_primitive_literal_expression(rhs);
            if can_drop_unused_const_literal {
                // Parity: upstream drops dead primitive const stores after propagation.
                // Preserve declaration tracking so later declaration checks stay coherent.
                cx.declare(&place.identifier);
                return None;
            }
            if kind == InstructionKind::Const
                && instr.lvalue.is_none()
                && cx.manual_memo_root_decls.contains(&decl_id)
                && is_inlineable_primitive_literal_expression(rhs)
            {
                // Upstream preserves manual-memo dependency roots as placeholder
                // declarations even when their initializer is constant-propagated away.
                cx.declare(&place.identifier);
                cx.mark_decl_runtime_emitted(decl_id);
                return render_reactive_variable_statement_ast(
                    ast::VariableDeclarationKind::Let,
                    &name,
                    None,
                );
            }
            let should_emit_fn_decl =
                kind == InstructionKind::Function || cx.function_decl_decls.contains(&decl_id);
            if should_emit_fn_decl && let Some(fn_decl) = function_expr_as_declaration(&name, rhs) {
                cx.declare(&place.identifier);
                cx.mark_decl_runtime_emitted(decl_id);
                return Some(format!("{fn_decl}\n"));
            }
            cx.declare(&place.identifier);
            cx.mark_decl_runtime_emitted(decl_id);
            render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Const,
                &name,
                Some(rhs),
            )
        }
        InstructionKind::Let | InstructionKind::HoistedLet => {
            let decl_id = place.identifier.declaration_id;
            if cx.function_decl_decls.contains(&decl_id)
                && let Some(fn_decl) = function_expr_as_declaration(&name, rhs)
            {
                cx.declare(&place.identifier);
                cx.mark_decl_runtime_emitted(decl_id);
                return Some(format!("{fn_decl}\n"));
            }
            cx.declare(&place.identifier);
            cx.mark_decl_runtime_emitted(decl_id);
            if rhs == "undefined" {
                render_reactive_variable_statement_ast(
                    ast::VariableDeclarationKind::Let,
                    &name,
                    None,
                )
            } else {
                render_reactive_variable_statement_ast(
                    ast::VariableDeclarationKind::Let,
                    &name,
                    Some(rhs),
                )
            }
        }
        InstructionKind::Reassign => {
            if instr.lvalue.is_none() && rhs.trim() == name {
                let is_context_store = matches!(instr.value, InstructionValue::StoreContext { .. });
                if !is_context_store {
                    return None;
                }
            }
            let expr = format!("{} = {}", name, rhs);
            if let Some(lvalue) = &instr.lvalue {
                if std::env::var("DEBUG_CODEGEN_STORE").is_ok() {
                    eprintln!(
                        "[CODEGEN_STORE] cached temp id={} expr={}",
                        lvalue.identifier.id.0, expr
                    );
                }
                // Store as temp for later reference
                if is_temp_like_identifier(cx, &lvalue.identifier) {
                    cx.inline_identifier_aliases
                        .insert(lvalue.identifier.declaration_id, expr.clone());
                }
                cx.set_temp_expr(
                    &lvalue.identifier,
                    Some(ExprValue::new(expr, ExprPrecedence::Assignment)),
                );
                None
            } else {
                render_reactive_assignment_statement_ast(&name, rhs)
            }
        }
        InstructionKind::Catch => {
            // Catch binding: `const e = t1;` inside handler body.
            // The catch temp is declared as the catch param, so this creates
            // a named alias for the catch parameter.
            cx.declare(&place.identifier);
            cx.mark_decl_runtime_emitted(place.identifier.declaration_id);
            render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Const,
                &name,
                Some(rhs),
            )
        }
        InstructionKind::HoistedConst | InstructionKind::HoistedFunction => {
            // Should have been pruned by PruneHoistedContexts
            cx.declare(&place.identifier);
            cx.mark_decl_runtime_emitted(place.identifier.declaration_id);
            render_reactive_variable_statement_ast(
                ast::VariableDeclarationKind::Const,
                &name,
                Some(rhs),
            )
        }
    }
}

fn codegen_instruction_expr_with_prec_kind(
    cx: &mut Context,
    instr: &ReactiveInstruction,
    value: &str,
    prec: ExprPrecedence,
    kind: ExprKind,
) -> Option<String> {
    if let Some(lvalue) = &instr.lvalue {
        if is_temp_like_identifier(cx, &lvalue.identifier) {
            // Temporary - store for later
            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                eprintln!(
                    "[CODEGEN_TEMP_SET] id={} decl={} expr={}",
                    lvalue.identifier.id.0, lvalue.identifier.declaration_id.0, value
                );
            }
            cx.inline_identifier_aliases
                .insert(lvalue.identifier.declaration_id, value.to_string());
            cx.set_temp_expr(
                &lvalue.identifier,
                Some(ExprValue {
                    expr: value.to_string(),
                    prec,
                    kind,
                }),
            );
            if let InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } = &instr.value
                && place.identifier.name.is_some()
            {
                let decl_id = place.identifier.declaration_id;
                let has_pending = cx.pending_manual_memo_reads.contains(&decl_id);
                let is_reassigned = cx.reassigned_decls.contains(&decl_id);
                if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                    eprintln!(
                        "[CODEGEN_TEMP_LOAD] decl={} name={:?} reassigned={} pending={}",
                        decl_id.0, place.identifier.name, is_reassigned, has_pending
                    );
                }
                if is_reassigned && has_pending {
                    cx.pending_manual_memo_reads.remove(&decl_id);
                }
            }
            None
        } else {
            let name = identifier_name_with_cx(cx, &lvalue.identifier);
            let rhs = if prec <= ExprPrecedence::Assignment {
                format!("({})", value)
            } else {
                value.to_string()
            };
            let has_runtime_binding = has_materialized_named_binding(cx, &lvalue.identifier);
            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                eprintln!(
                    "[CODEGEN_NAMED_LVALUE] id={} decl={} name={} declared={} runtime={} elided={} rhs={}",
                    lvalue.identifier.id.0,
                    lvalue.identifier.declaration_id.0,
                    name,
                    cx.has_declared(&lvalue.identifier),
                    cx.has_declared_by_runtime_emission(lvalue.identifier.declaration_id),
                    cx.elided_named_declarations
                        .contains(&lvalue.identifier.declaration_id),
                    rhs
                );
            }
            if has_runtime_binding {
                let mut emitted = String::new();
                if let Some(prefix) =
                    maybe_materialize_elided_named_declaration(cx, &lvalue.identifier)
                {
                    emitted.push_str(&prefix);
                }
                emitted.push_str(&render_reactive_assignment_statement_ast(&name, &rhs)?);
                Some(emitted)
            } else {
                cx.declare(&lvalue.identifier);
                cx.mark_decl_runtime_emitted(lvalue.identifier.declaration_id);
                let decl_kind =
                    if matches!(lvalue.identifier.name, Some(IdentifierName::Promoted(_))) {
                        ast::VariableDeclarationKind::Let
                    } else {
                        ast::VariableDeclarationKind::Const
                    };
                render_reactive_variable_statement_ast(decl_kind, &name, Some(&rhs))
            }
        }
    } else {
        if value == "undefined" {
            None
        } else if let InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. } = &instr.value
        {
            if place.identifier.name.is_none()
                && (is_inlineable_primitive_literal_expression(value)
                    || is_simple_identifier_expression(value))
            {
                // Preserve upstream parity: unnamed-temp literal loads in statement
                // position are dead no-ops after lowering and should be elided.
                None
            } else {
                debug_codegen_expr(
                    "stmt-expr-emit",
                    format!(
                        "kind={} expr={}",
                        instruction_value_tag(&instr.value),
                        value
                    ),
                );
                render_reactive_expression_statement_ast(value)
            }
        } else {
            debug_codegen_expr(
                "stmt-expr-emit",
                format!(
                    "kind={} expr={}",
                    instruction_value_tag(&instr.value),
                    value
                ),
            );
            render_reactive_expression_statement_ast(value)
        }
    }
}

/// Generate an ExprValue from an instruction value (with proper precedence tracking).
fn codegen_instruction_value_ev(cx: &mut Context, value: &InstructionValue) -> ExprValue {
    match value {
        InstructionValue::Primitive { value: prim, .. } => {
            ExprValue::primary(codegen_primitive(prim))
        }
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            codegen_place_expr_value(cx, place)
        }
        InstructionValue::LoadGlobal { binding, .. } => {
            ExprValue::primary(binding.name().to_string())
        }
        InstructionValue::BinaryExpression {
            left,
            right,
            operator,
            ..
        } => {
            let prec = binary_operator_precedence(operator);
            let l = codegen_place_with_min_prec(cx, left, prec);
            let r = codegen_place_with_min_prec(cx, right, prec);
            let expr = render_binary_expression_ast(&l, *operator, &r)
                .unwrap_or_else(|| format!("{} {} {}", l, operator_to_str(operator), r));
            ExprValue::new(expr, prec)
        }
        InstructionValue::UnaryExpression {
            value: operand,
            operator,
            ..
        } => {
            let expr = codegen_place_with_min_prec(cx, operand, ExprPrecedence::Unary);
            let op = unary_operator_to_str(operator);
            let rendered = render_unary_expression_ast(*operator, &expr).unwrap_or_else(|| {
                if op.chars().all(|c| c.is_alphanumeric()) {
                    format!("{} {}", op, expr)
                } else {
                    format!("{}{}", op, expr)
                }
            });
            ExprValue::new(rendered, ExprPrecedence::Unary)
        }
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => {
            let is_hook_call = resolve_place_name(cx, callee)
                .as_deref()
                .is_some_and(|name| extract_hook_name(name).is_some());
            let callee_expr = codegen_place_to_expression(cx, callee);
            let mut rendered_args: Vec<String> =
                args.iter().map(|a| codegen_argument(cx, a)).collect();
            maybe_replace_autodeps_with_inferred_deps(
                cx,
                &callee_expr,
                args,
                &mut rendered_args,
                true,
            );
            let args_str = join_call_arguments(&rendered_args);
            // Wrap arrow/functions in parens for IIFE callee position.
            let callee_trimmed = callee_expr.trim_start();
            let needs_wrap = callee_expr.contains("=>")
                || callee_trimmed.starts_with("function ")
                || callee_trimmed.starts_with("function*")
                || callee_trimmed.starts_with("async function ")
                || callee_trimmed.starts_with("async function*");
            let callee_final = if needs_wrap {
                format!("({})", callee_expr)
            } else {
                callee_expr
            };
            debug_codegen_expr(
                "call-expression",
                format!(
                    "callee={} optional={} args={:?}",
                    callee_final, optional, rendered_args
                ),
            );
            let call_expr = render_call_expression_ast(&callee_final, args, &rendered_args, *optional)
                .unwrap_or_else(|| {
                    if *optional {
                        if should_break_optional_call_args(&rendered_args) {
                            format!("{}?.(\n{})", callee_final, args_str)
                        } else {
                            format!("{}?.({})", callee_final, args_str)
                        }
                    } else {
                        format!("{}({})", callee_final, args_str)
                    }
                });
            if cx.emit_hook_guards && is_hook_call {
                ExprValue::primary(wrap_hook_guarded_call_expression(&call_expr))
            } else {
                ExprValue::primary(call_expr)
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
            let recv = codegen_member_object_expression(cx, receiver);
            let (prop, is_computed) = resolve_method_property(cx, property, &recv);
            let mut rendered_args: Vec<String> =
                args.iter().map(|a| codegen_argument(cx, a)).collect();
            if std::env::var("DEBUG_METHODCALL").is_ok() {
                let arg_decls: Vec<u32> = args
                    .iter()
                    .map(|a| match a {
                        Argument::Place(p) | Argument::Spread(p) => p.identifier.declaration_id.0,
                    })
                    .collect();
                eprintln!(
                    "[METHODCALL] recv={} prop={} computed={} args_decl={:?} rendered_args={:?}",
                    recv, prop, is_computed, arg_decls, rendered_args
                );
            }
            let hook_name = if is_computed {
                None
            } else {
                Some(prop.as_str())
            };
            maybe_replace_autodeps_with_inferred_deps(
                cx,
                hook_name.unwrap_or(""),
                args,
                &mut rendered_args,
                hook_name.is_some(),
            );
            let args_str = join_call_arguments(&rendered_args);
            debug_codegen_expr(
                "method-call",
                format!(
                    "receiver={} property={} computed={} recv_optional={} call_optional={} args={:?}",
                    recv, prop, is_computed, receiver_optional, call_optional, rendered_args
                ),
            );
            let is_hook_call = !is_computed && Environment::is_hook_name(&prop);
            let call_expr = if is_computed {
                let opt_recv = if *receiver_optional { "?." } else { "" };
                if *call_optional {
                    if should_break_optional_call_args(&rendered_args) {
                        format!("{}{}[{}]?.(\n{})", recv, opt_recv, prop, args_str)
                    } else {
                        format!("{}{}[{}]?.({})", recv, opt_recv, prop, args_str)
                    }
                } else {
                    format!("{}{}[{}]({})", recv, opt_recv, prop, args_str)
                }
            } else {
                let dot = if *receiver_optional { "?." } else { "." };
                if *call_optional {
                    if should_break_optional_call_args(&rendered_args) {
                        format!("{}{}{}?.(\n{})", recv, dot, prop, args_str)
                    } else {
                        format!("{}{}{}?.({})", recv, dot, prop, args_str)
                    }
                } else {
                    if !*receiver_optional
                        && recv.starts_with("new ")
                        && (recv.contains('\n')
                            || prop == "build"
                            || recv.matches('.').count() >= 1)
                    {
                        format!("{}\n.{}({})", recv, prop, args_str)
                    } else {
                        format!("{}{}{}({})", recv, dot, prop, args_str)
                    }
                }
            };
            if cx.emit_hook_guards && is_hook_call {
                ExprValue::primary(wrap_hook_guarded_call_expression(&call_expr))
            } else {
                ExprValue::primary(call_expr)
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => {
            let callee_expr = codegen_place_to_expression(cx, callee);
            let args_str = args
                .iter()
                .map(|a| codegen_argument(cx, a))
                .collect::<Vec<_>>()
                .join(", ");
            let expr = render_new_expression_ast(cx, callee, args)
                .unwrap_or_else(|| format!("new {}({})", callee_expr, args_str));
            ExprValue::primary(expr)
        }
        InstructionValue::ObjectExpression { properties, .. } => {
            if properties.is_empty() {
                return ExprValue::primary("{}".to_string());
            }
            let props: Vec<String> = properties
                .iter()
                .map(|p| codegen_object_property(cx, p))
                .collect();
            let expr = render_object_expression_ast(cx, properties)
                .unwrap_or_else(|| format!("{{ {} }}", props.join(", ")));
            ExprValue::primary(expr)
        }
        InstructionValue::ArrayExpression { elements, .. } => {
            let elems: Vec<String> = elements
                .iter()
                .map(|e| match e {
                    ArrayElement::Place(p) => codegen_place_to_expression(cx, p),
                    ArrayElement::Spread(p) => format!("...{}", codegen_place_to_expression(cx, p)),
                    ArrayElement::Hole => String::new(),
                })
                .collect();
            let expr = render_array_expression_ast(cx, elements)
                .unwrap_or_else(|| format!("[{}]", elems.join(", ")));
            ExprValue::primary(expr)
        }
        InstructionValue::PropertyLoad {
            object,
            property,
            optional,
            ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            ExprValue::primary(format_property_access(&obj, property, *optional))
        }
        InstructionValue::PropertyStore {
            object,
            property,
            value: val,
            ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            let v = codegen_place_to_expression(cx, val);
            let expr = render_property_store_expression_ast(&obj, property, &v)
                .unwrap_or_else(|| format!("{} = {}", format_property_access(&obj, property, false), v));
            ExprValue::new(expr, ExprPrecedence::Assignment)
        }
        InstructionValue::PropertyDelete {
            object, property, ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            let expr = render_property_delete_expression_ast(&obj, property).unwrap_or_else(|| {
                format!("delete {}", format_property_access(&obj, property, false))
            });
            ExprValue::new(expr, ExprPrecedence::Unary)
        }
        InstructionValue::ComputedLoad {
            object,
            property,
            optional,
            ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            let prop = codegen_place_to_expression(cx, property);
            let expr = render_computed_access_expression_ast(&obj, &prop, *optional)
                .unwrap_or_else(|| {
                    if *optional {
                        format!("{}?.[{}]", obj, prop)
                    } else {
                        format!("{}[{}]", obj, prop)
                    }
                });
            ExprValue::primary(expr)
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: val,
            ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            let prop = codegen_place_to_expression(cx, property);
            let v = codegen_place_to_expression(cx, val);
            let expr = render_computed_store_expression_ast(&obj, &prop, &v)
                .unwrap_or_else(|| format!("{}[{}] = {}", obj, prop, v));
            ExprValue::new(expr, ExprPrecedence::Assignment)
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            let obj = codegen_member_object_expression(cx, object);
            let prop = codegen_place_to_expression(cx, property);
            let expr = render_computed_delete_expression_ast(&obj, &prop)
                .unwrap_or_else(|| format!("delete {}[{}]", obj, prop));
            ExprValue::new(expr, ExprPrecedence::Unary)
        }
        InstructionValue::StoreGlobal {
            name, value: val, ..
        } => {
            let v = codegen_place_to_expression(cx, val);
            let expr = render_global_store_expression_ast(name, &v)
                .unwrap_or_else(|| format!("{} = {}", name, v));
            ExprValue::new(expr, ExprPrecedence::Assignment)
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => ExprValue::primary(codegen_jsx(cx, tag, props, children)),
        InstructionValue::JsxFragment { children, .. } => {
            ExprValue::primary(codegen_jsx_fragment(cx, children))
        }
        InstructionValue::JSXText { value: text, .. } => {
            ExprValue::jsx_text(format!("\"{}\"", escape_jsx_text(text)))
        }
        InstructionValue::FunctionExpression {
            lowered_func,
            name,
            expr_type,
            ..
        } => ExprValue::primary(codegen_function_expression(
            cx,
            lowered_func,
            name,
            expr_type,
        )),
        InstructionValue::TaggedTemplateExpression {
            tag, raw, cooked, ..
        } => {
            let tag_expr = codegen_place_to_expression(cx, tag);
            let expr = render_tagged_template_expression_ast(cx, tag, raw, cooked.as_deref())
                .unwrap_or_else(|| format!("{}`{}`", tag_expr, raw));
            ExprValue::primary(expr)
        }
        InstructionValue::TemplateLiteral {
            quasis, subexprs, ..
        } => {
            let expr = render_template_literal_ast(cx, quasis, subexprs)
                .unwrap_or_else(|| codegen_template_literal(cx, quasis, subexprs));
            ExprValue::primary(expr)
        }
        InstructionValue::TypeCastExpression {
            value: val,
            type_annotation,
            type_annotation_kind,
            ..
        } => {
            let value_expr =
                codegen_place_expr_value(cx, val).wrap_if_needed(match type_annotation_kind {
                    TypeAnnotationKind::Cast => ExprPrecedence::Assignment,
                    TypeAnnotationKind::As | TypeAnnotationKind::Satisfies => {
                        ExprPrecedence::Relational
                    }
                });
            let expr = match type_annotation_kind {
                TypeAnnotationKind::Cast => format!("({}: {})", value_expr, type_annotation),
                TypeAnnotationKind::As => format!("{} as {}", value_expr, type_annotation),
                TypeAnnotationKind::Satisfies => {
                    format!("{} satisfies {}", value_expr, type_annotation)
                }
            };
            ExprValue::primary(expr)
        }
        InstructionValue::RegExpLiteral { pattern, flags, .. } => {
            ExprValue::primary(format!("/{}/{}", pattern, flags))
        }
        InstructionValue::MetaProperty { meta, property, .. } => {
            let expr = render_meta_property_expression_ast(meta, property)
                .unwrap_or_else(|| format!("{}.{}", meta, property));
            ExprValue::primary(expr)
        }
        InstructionValue::Await { value: val, .. } => {
            let expr = codegen_place_to_expression(cx, val);
            let rendered = render_await_expression_ast(&expr)
                .unwrap_or_else(|| format!("await {}", expr));
            ExprValue::new(rendered, ExprPrecedence::Unary)
        }
        InstructionValue::GetIterator { collection, .. } => {
            codegen_place_expr_value(cx, collection)
        }
        InstructionValue::IteratorNext { iterator, .. } => codegen_place_expr_value(cx, iterator),
        InstructionValue::NextPropertyOf { value: val, .. } => codegen_place_expr_value(cx, val),
        InstructionValue::PostfixUpdate {
            value, operation, ..
        } => {
            // Use `value` (read operand) rather than `lvalue` (write operand).
            // After SSA, `lvalue` gets a new IdentifierId that has no entry in
            // cx.temp, so codegen would fall back to a synthetic "tN" name.
            // `value` still maps to the LoadLocal that loaded the original variable.
            let expr = codegen_place_to_expression(cx, value);
            let rendered = render_update_expression_ast(&expr, *operation, false)
                .unwrap_or_else(|| format!("{}{}", expr, update_op_to_str(operation)));
            ExprValue::primary(rendered)
        }
        InstructionValue::PrefixUpdate {
            value, operation, ..
        } => {
            let expr = codegen_place_to_expression(cx, value);
            let rendered = render_update_expression_ast(&expr, *operation, true)
                .unwrap_or_else(|| format!("{}{}", update_op_to_str(operation), expr));
            ExprValue::primary(rendered)
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => codegen_reactive_sequence_expression_ev(cx, instructions, value),
        InstructionValue::ReactiveOptionalExpression {
            optional, value, ..
        } => {
            let inner_ev = codegen_instruction_value_ev(cx, value);
            if let Some(expr) = apply_optional_to_rendered_expr(&inner_ev.expr, *optional) {
                ExprValue::primary(expr)
            } else {
                set_codegen_error_once(
                    cx,
                    "Expected an optional value to resolve to a call expression or member expression",
                    inner_ev.expr.clone(),
                );
                inner_ev
            }
        }
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => {
            let test_ev = codegen_instruction_value_ev(cx, test);
            let test_str = test_ev.wrap_if_needed(ExprPrecedence::Conditional);
            let cons_ev = codegen_instruction_value_ev(cx, consequent);
            let alt_ev = codegen_instruction_value_ev(cx, alternate);
            let cons_str = if needs_parens_in_ternary_branch(&cons_ev) {
                format!("({})", cons_ev.expr)
            } else {
                cons_ev.expr
            };
            let alt_str = if needs_parens_in_ternary_branch(&alt_ev) {
                format!("({})", alt_ev.expr)
            } else {
                alt_ev.expr
            };
            let expr = render_conditional_expression_ast(&test_str, &cons_str, &alt_str)
                .unwrap_or_else(|| format!("{} ? {} : {}", test_str, cons_str, alt_str));
            ExprValue::new(expr, ExprPrecedence::Conditional)
        }
        InstructionValue::ReactiveLogicalExpression {
            operator,
            left,
            right,
            ..
        } => {
            let prec = logical_operator_precedence(operator);
            let l = codegen_logical_operand_from_expr_value(
                codegen_instruction_value_ev(cx, left),
                prec,
            );
            let mut r = codegen_logical_operand_from_expr_value(
                codegen_instruction_value_ev(cx, right),
                prec,
            );
            if *operator == LogicalOperator::NullishCoalescing {
                let rhs_trimmed = r.trim();
                if rhs_trimmed.starts_with('<')
                    && !(rhs_trimmed.starts_with('(') && rhs_trimmed.ends_with(')'))
                {
                    r = format!("(\n{}\n)", rhs_trimmed);
                }
            }
            let expr = render_logical_expression_ast(&l, *operator, &r)
                .unwrap_or_else(|| format!("{} {} {}", l, logical_operator_to_str(operator), r));
            ExprValue::new(expr, prec)
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            let test_str = codegen_place_with_min_prec(cx, test, ExprPrecedence::Conditional);
            let cons_ev = codegen_place_expr_value(cx, consequent);
            let alt_ev = codegen_place_expr_value(cx, alternate);
            let cons_str = if needs_parens_in_ternary_branch(&cons_ev) {
                format!("({})", cons_ev.expr)
            } else {
                cons_ev.expr
            };
            let alt_str = if needs_parens_in_ternary_branch(&alt_ev) {
                format!("({})", alt_ev.expr)
            } else {
                alt_ev.expr
            };
            let expr = render_conditional_expression_ast(&test_str, &cons_str, &alt_str)
                .unwrap_or_else(|| format!("{} ? {} : {}", test_str, cons_str, alt_str));
            ExprValue::new(expr, ExprPrecedence::Conditional)
        }
        InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } => {
            let prec = logical_operator_precedence(operator);
            let l = codegen_logical_operand(cx, left, prec);
            let mut r = codegen_logical_operand(cx, right, prec);
            if *operator == LogicalOperator::NullishCoalescing {
                let rhs_trimmed = r.trim();
                if rhs_trimmed.starts_with('<')
                    && !(rhs_trimmed.starts_with('(') && rhs_trimmed.ends_with(')'))
                {
                    r = format!("(\n{}\n)", rhs_trimmed);
                }
            }
            let expr = render_logical_expression_ast(&l, *operator, &r)
                .unwrap_or_else(|| format!("{} {} {}", l, logical_operator_to_str(operator), r));
            ExprValue::new(expr, prec)
        }
        // These should be handled in codegen_instruction_nullable directly
        InstructionValue::StoreLocal { .. }
        | InstructionValue::StoreContext { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::Destructure { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. }
        | InstructionValue::Debugger { .. }
        | InstructionValue::ObjectMethod { .. } => {
            // Should not reach here
            ExprValue::primary("/* unexpected */".to_string())
        }
    }
}

// ---- Precedence helpers ----

fn binary_operator_precedence(op: &BinaryOperator) -> ExprPrecedence {
    match op {
        BinaryOperator::Eq
        | BinaryOperator::NotEq
        | BinaryOperator::StrictEq
        | BinaryOperator::StrictNotEq => ExprPrecedence::Equality,
        BinaryOperator::Lt
        | BinaryOperator::LtEq
        | BinaryOperator::Gt
        | BinaryOperator::GtEq
        | BinaryOperator::InstanceOf
        | BinaryOperator::In => ExprPrecedence::Relational,
        BinaryOperator::Add | BinaryOperator::Sub => ExprPrecedence::Additive,
        BinaryOperator::Mul | BinaryOperator::Div | BinaryOperator::Mod => {
            ExprPrecedence::Multiplicative
        }
        BinaryOperator::Exp => ExprPrecedence::Exponentiation,
        BinaryOperator::LShift | BinaryOperator::RShift | BinaryOperator::URShift => {
            ExprPrecedence::Shift
        }
        BinaryOperator::BitAnd => ExprPrecedence::BitwiseAnd,
        BinaryOperator::BitOr => ExprPrecedence::BitwiseOr,
        BinaryOperator::BitXor => ExprPrecedence::BitwiseXor,
    }
}

fn logical_operator_precedence(op: &LogicalOperator) -> ExprPrecedence {
    match op {
        LogicalOperator::And => ExprPrecedence::LogicalAnd,
        LogicalOperator::Or => ExprPrecedence::LogicalOr,
        LogicalOperator::NullishCoalescing => ExprPrecedence::NullishCoalescing,
    }
}

fn logical_operator_to_str(op: &LogicalOperator) -> &'static str {
    match op {
        LogicalOperator::And => "&&",
        LogicalOperator::Or => "||",
        LogicalOperator::NullishCoalescing => "??",
    }
}

fn codegen_logical_operand_from_expr_value(ev: ExprValue, parent_prec: ExprPrecedence) -> String {
    let is_logical = matches!(
        ev.prec,
        ExprPrecedence::LogicalAnd | ExprPrecedence::LogicalOr | ExprPrecedence::NullishCoalescing
    );
    if is_logical && ev.prec != parent_prec {
        format!("({})", ev.expr)
    } else {
        ev.wrap_if_needed(parent_prec)
    }
}

fn codegen_logical_operand(cx: &mut Context, place: &Place, parent_prec: ExprPrecedence) -> String {
    codegen_logical_operand_from_expr_value(codegen_place_expr_value(cx, place), parent_prec)
}

fn needs_parens_in_ternary_branch(ev: &ExprValue) -> bool {
    ev.prec <= ExprPrecedence::NullishCoalescing
        || ev.expr.contains(" as ")
        || ev.expr.contains(" satisfies ")
}

fn codegen_reactive_sequence_expression_ev(
    cx: &mut Context,
    instructions: &[ReactiveInstruction],
    value: &InstructionValue,
) -> ExprValue {
    let mut probe_cx = cx.clone();
    let mut prefix_exprs = Vec::new();
    let uses_decl = |value: &InstructionValue, decl_id: DeclarationId| {
        let mut used = false;
        crate::hir::visitors::for_each_instruction_value_operand(value, |place| {
            if place.identifier.declaration_id == decl_id {
                used = true;
            }
        });
        used
    };
    for (idx, instr) in instructions.iter().enumerate() {
        match codegen_instruction_nullable(&mut probe_cx, instr) {
            Some(stmt) => {
                if let Some(expr) = extract_simple_expression_statement_global(&stmt) {
                    let wrapped = if is_assignment_like_sequence_expr_global(&expr)
                        && !(expr.starts_with('(') && expr.ends_with(')'))
                    {
                        format!("({expr})")
                    } else {
                        expr
                    };
                    prefix_exprs.push(wrapped);
                } else {
                    set_codegen_error_once(
                        cx,
                        "Cannot declare variables in a value block",
                        stmt.trim().to_string(),
                    );
                    return ExprValue::primary("/* unexpected value block */".to_string());
                }
            }
            None => {
                let Some(lvalue) = &instr.lvalue else {
                    continue;
                };
                if !is_temp_like_identifier(&probe_cx, &lvalue.identifier) {
                    continue;
                }
                let decl_id = lvalue.identifier.declaration_id;
                let used_later = instructions[idx + 1..]
                    .iter()
                    .any(|later| uses_decl(&later.value, decl_id))
                    || uses_decl(value, decl_id);
                if used_later {
                    continue;
                }
                let maybe_expr = match &instr.value {
                    InstructionValue::StoreLocal {
                        lvalue: store_lvalue,
                        value: store_value,
                        ..
                    } => {
                        let lhs = codegen_place_to_expression(&mut probe_cx, &store_lvalue.place);
                        let rhs = codegen_place_to_expression(&mut probe_cx, store_value);
                        Some(format!("{lhs} = {rhs}"))
                    }
                    InstructionValue::StoreContext {
                        lvalue: store_lvalue,
                        value: store_value,
                        ..
                    } => {
                        let lhs = codegen_place_to_expression(&mut probe_cx, &store_lvalue.place);
                        let rhs = codegen_place_to_expression(&mut probe_cx, store_value);
                        Some(format!("{lhs} = {rhs}"))
                    }
                    InstructionValue::CallExpression { .. }
                    | InstructionValue::MethodCall { .. }
                    | InstructionValue::PropertyStore { .. }
                    | InstructionValue::ComputedStore { .. }
                    | InstructionValue::PropertyDelete { .. }
                    | InstructionValue::ComputedDelete { .. }
                    | InstructionValue::PrefixUpdate { .. }
                    | InstructionValue::PostfixUpdate { .. } => {
                        Some(codegen_instruction_value_ev(&mut probe_cx, &instr.value).expr)
                    }
                    _ => None,
                };
                if let Some(expr) = maybe_expr {
                    let wrapped = if is_assignment_like_sequence_expr_global(&expr)
                        && !(expr.starts_with('(') && expr.ends_with(')'))
                    {
                        format!("({expr})")
                    } else {
                        expr
                    };
                    prefix_exprs.push(wrapped);
                }
            }
        }
    }
    let final_ev = codegen_instruction_value_ev(&mut probe_cx, value);
    adopt_codegen_error(cx, probe_cx.codegen_error.take());
    if prefix_exprs.is_empty() {
        final_ev
    } else {
        ExprValue::primary(wrap_sequence_expr(&prefix_exprs, final_ev.expr))
    }
}

fn apply_optional_to_rendered_expr(expr: &str, optional: bool) -> Option<String> {
    if !optional {
        return Some(expr.to_string());
    }
    let trimmed = expr.trim();
    if trimmed.contains("?.") {
        return Some(trimmed.to_string());
    }
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), trimmed)
            .ok()?;
    let expression = match parsed {
        ast::Expression::CallExpression(call) => {
            let mut call = call.unbox();
            call.optional = true;
            builder.expression_chain(
                SPAN,
                ast::ChainElement::CallExpression(builder.alloc(call)),
            )
        }
        ast::Expression::ComputedMemberExpression(member) => {
            let mut member = member.unbox();
            member.optional = true;
            builder.expression_chain(
                SPAN,
                ast::ChainElement::ComputedMemberExpression(builder.alloc(member)),
            )
        }
        ast::Expression::StaticMemberExpression(member) => {
            let mut member = member.unbox();
            member.optional = true;
            builder.expression_chain(
                SPAN,
                ast::ChainElement::StaticMemberExpression(builder.alloc(member)),
            )
        }
        ast::Expression::PrivateFieldExpression(member) => {
            let mut member = member.unbox();
            member.optional = true;
            builder.expression_chain(
                SPAN,
                ast::ChainElement::PrivateFieldExpression(builder.alloc(member)),
            )
        }
        _ => return None,
    };
    Some(strip_top_level_parenthesized_expression(
        codegen_expression_with_oxc(&expression),
    ))
}

fn parse_rendered_expression_ast<'a>(
    allocator: &'a Allocator,
    expr: &str,
) -> Option<ast::Expression<'a>> {
    parse_expression_for_ast_codegen(allocator, SourceType::mjs().with_jsx(true), expr).ok()
}

fn lower_binary_operator_ast(operator: BinaryOperator) -> AstBinaryOperator {
    match operator {
        BinaryOperator::Eq => AstBinaryOperator::Equality,
        BinaryOperator::NotEq => AstBinaryOperator::Inequality,
        BinaryOperator::StrictEq => AstBinaryOperator::StrictEquality,
        BinaryOperator::StrictNotEq => AstBinaryOperator::StrictInequality,
        BinaryOperator::Lt => AstBinaryOperator::LessThan,
        BinaryOperator::LtEq => AstBinaryOperator::LessEqualThan,
        BinaryOperator::Gt => AstBinaryOperator::GreaterThan,
        BinaryOperator::GtEq => AstBinaryOperator::GreaterEqualThan,
        BinaryOperator::LShift => AstBinaryOperator::ShiftLeft,
        BinaryOperator::RShift => AstBinaryOperator::ShiftRight,
        BinaryOperator::URShift => AstBinaryOperator::ShiftRightZeroFill,
        BinaryOperator::Add => AstBinaryOperator::Addition,
        BinaryOperator::Sub => AstBinaryOperator::Subtraction,
        BinaryOperator::Mul => AstBinaryOperator::Multiplication,
        BinaryOperator::Div => AstBinaryOperator::Division,
        BinaryOperator::Mod => AstBinaryOperator::Remainder,
        BinaryOperator::Exp => AstBinaryOperator::Exponential,
        BinaryOperator::BitOr => AstBinaryOperator::BitwiseOR,
        BinaryOperator::BitXor => AstBinaryOperator::BitwiseXOR,
        BinaryOperator::BitAnd => AstBinaryOperator::BitwiseAnd,
        BinaryOperator::In => AstBinaryOperator::In,
        BinaryOperator::InstanceOf => AstBinaryOperator::Instanceof,
    }
}

fn lower_unary_operator_ast(operator: UnaryOperator) -> AstUnaryOperator {
    match operator {
        UnaryOperator::Minus => AstUnaryOperator::UnaryNegation,
        UnaryOperator::Plus => AstUnaryOperator::UnaryPlus,
        UnaryOperator::Not => AstUnaryOperator::LogicalNot,
        UnaryOperator::BitNot => AstUnaryOperator::BitwiseNot,
        UnaryOperator::TypeOf => AstUnaryOperator::Typeof,
        UnaryOperator::Void => AstUnaryOperator::Void,
    }
}

fn lower_logical_operator_ast(operator: LogicalOperator) -> AstLogicalOperator {
    match operator {
        LogicalOperator::And => AstLogicalOperator::And,
        LogicalOperator::Or => AstLogicalOperator::Or,
        LogicalOperator::NullishCoalescing => AstLogicalOperator::Coalesce,
    }
}

fn lower_update_operator_ast(operator: UpdateOperator) -> AstUpdateOperator {
    match operator {
        UpdateOperator::Increment => AstUpdateOperator::Increment,
        UpdateOperator::Decrement => AstUpdateOperator::Decrement,
    }
}

fn render_binary_expression_ast(
    left: &str,
    operator: BinaryOperator,
    right: &str,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let left = parse_rendered_expression_ast(&allocator, left)?;
    let right = parse_rendered_expression_ast(&allocator, right)?;
    let expression =
        builder.expression_binary(SPAN, left, lower_binary_operator_ast(operator), right);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_unary_expression_ast(operator: UnaryOperator, value: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let value = parse_rendered_expression_ast(&allocator, value)?;
    let expression = builder.expression_unary(SPAN, lower_unary_operator_ast(operator), value);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_logical_expression_ast(
    left: &str,
    operator: LogicalOperator,
    right: &str,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let left = parse_rendered_expression_ast(&allocator, left)?;
    let right = parse_rendered_expression_ast(&allocator, right)?;
    let expression =
        builder.expression_logical(SPAN, left, lower_logical_operator_ast(operator), right);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_conditional_expression_ast(
    test: &str,
    consequent: &str,
    alternate: &str,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let test = parse_rendered_expression_ast(&allocator, test)?;
    let consequent = parse_rendered_expression_ast(&allocator, consequent)?;
    let alternate = parse_rendered_expression_ast(&allocator, alternate)?;
    let expression = builder.expression_conditional(SPAN, test, consequent, alternate);
    Some(codegen_expression_with_oxc(&expression))
}

fn expression_to_simple_assignment_target_ast<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::SimpleAssignmentTarget<'a>> {
    match expression {
        ast::Expression::Identifier(identifier) => Some(
            builder.simple_assignment_target_assignment_target_identifier(SPAN, identifier.name),
        ),
        ast::Expression::ComputedMemberExpression(member) => {
            Some(ast::SimpleAssignmentTarget::from(
                ast::MemberExpression::ComputedMemberExpression(member),
            ))
        }
        ast::Expression::StaticMemberExpression(member) => Some(ast::SimpleAssignmentTarget::from(
            ast::MemberExpression::StaticMemberExpression(member),
        )),
        ast::Expression::PrivateFieldExpression(member) => Some(ast::SimpleAssignmentTarget::from(
            ast::MemberExpression::PrivateFieldExpression(member),
        )),
        _ => None,
    }
}

fn expression_to_assignment_target_ast<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
) -> Option<ast::AssignmentTarget<'a>> {
    Some(ast::AssignmentTarget::from(
        expression_to_simple_assignment_target_ast(builder, expression)?,
    ))
}

fn build_property_access_expression_ast<'a>(
    builder: AstBuilder<'a>,
    object: ast::Expression<'a>,
    property: &PropertyLiteral,
    optional: bool,
) -> ast::Expression<'a> {
    match property {
        PropertyLiteral::String(name) if is_non_negative_integer_string(name) => {
            ast::Expression::from(builder.member_expression_computed(
                SPAN,
                object,
                builder.expression_numeric_literal(
                    SPAN,
                    name.parse::<f64>().ok().unwrap_or_default(),
                    None,
                    NumberBase::Decimal,
                ),
                optional,
            ))
        }
        PropertyLiteral::String(name) if is_valid_js_identifier(name) => {
            ast::Expression::from(builder.member_expression_static(
                SPAN,
                object,
                builder.identifier_name(SPAN, builder.ident(name)),
                optional,
            ))
        }
        PropertyLiteral::String(name) => ast::Expression::from(builder.member_expression_computed(
            SPAN,
            object,
            builder.expression_string_literal(SPAN, builder.atom(name), None),
            optional,
        )),
        PropertyLiteral::Number(value) => ast::Expression::from(builder.member_expression_computed(
            SPAN,
            object,
            builder.expression_numeric_literal(SPAN, *value, None, NumberBase::Decimal),
            optional,
        )),
    }
}

fn render_property_store_expression_ast(
    object: &str,
    property: &PropertyLiteral,
    value: &str,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let object = parse_rendered_expression_ast(&allocator, object)?;
    let value = parse_rendered_expression_ast(&allocator, value)?;
    let target = expression_to_assignment_target_ast(
        builder,
        build_property_access_expression_ast(builder, object, property, false),
    )?;
    let expression = builder.expression_assignment(SPAN, AssignmentOperator::Assign, target, value);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_property_delete_expression_ast(object: &str, property: &PropertyLiteral) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let object = parse_rendered_expression_ast(&allocator, object)?;
    let member = build_property_access_expression_ast(builder, object, property, false);
    let expression = builder.expression_unary(SPAN, AstUnaryOperator::Delete, member);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_computed_access_expression_ast(
    object: &str,
    property: &str,
    optional: bool,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let object = parse_rendered_expression_ast(&allocator, object)?;
    let property = parse_rendered_expression_ast(&allocator, property)?;
    let expression = ast::Expression::from(
        builder.member_expression_computed(SPAN, object, property, optional),
    );
    Some(codegen_expression_with_oxc(&expression))
}

fn render_computed_store_expression_ast(object: &str, property: &str, value: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let object = parse_rendered_expression_ast(&allocator, object)?;
    let property = parse_rendered_expression_ast(&allocator, property)?;
    let value = parse_rendered_expression_ast(&allocator, value)?;
    let target = ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
        builder.member_expression_computed(SPAN, object, property, false),
    ));
    let expression = builder.expression_assignment(SPAN, AssignmentOperator::Assign, target, value);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_computed_delete_expression_ast(object: &str, property: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let object = parse_rendered_expression_ast(&allocator, object)?;
    let property = parse_rendered_expression_ast(&allocator, property)?;
    let expression = builder.expression_unary(
        SPAN,
        AstUnaryOperator::Delete,
        ast::Expression::from(builder.member_expression_computed(SPAN, object, property, false)),
    );
    Some(codegen_expression_with_oxc(&expression))
}

fn render_global_store_expression_ast(name: &str, value: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let value = parse_rendered_expression_ast(&allocator, value)?;
    let target = ast::AssignmentTarget::from(
        builder.simple_assignment_target_assignment_target_identifier(SPAN, builder.ident(name)),
    );
    let expression = builder.expression_assignment(SPAN, AssignmentOperator::Assign, target, value);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_update_expression_ast(
    target: &str,
    operator: UpdateOperator,
    prefix: bool,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let target = parse_rendered_expression_ast(&allocator, target)?;
    let target = expression_to_simple_assignment_target_ast(builder, target)?;
    let expression = builder.expression_update(
        SPAN,
        lower_update_operator_ast(operator),
        prefix,
        target,
    );
    Some(codegen_expression_with_oxc(&expression))
}

fn render_new_expression_ast(
    cx: &mut Context,
    callee: &Place,
    args: &[Argument],
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let callee = parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, callee))?;
    let mut lowered_args = builder.vec();
    for arg in args {
        match arg {
            Argument::Place(place) => lowered_args.push(ast::Argument::from(
                parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, place))?,
            )),
            Argument::Spread(place) => lowered_args.push(builder.argument_spread_element(
                SPAN,
                parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, place))?,
            )),
        }
    }
    let expression = builder.expression_new(SPAN, callee, NONE, lowered_args);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_arguments_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    args: &[Argument],
    rendered_args: &[String],
) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
    if args.len() != rendered_args.len() {
        return None;
    }
    let mut lowered = builder.vec();
    for (arg, rendered) in args.iter().zip(rendered_args) {
        match arg {
            Argument::Place(_) => lowered.push(ast::Argument::from(
                parse_rendered_expression_ast(allocator, rendered)?,
            )),
            Argument::Spread(_) => {
                let expr = rendered.strip_prefix("...").unwrap_or(rendered);
                lowered.push(builder.argument_spread_element(
                    SPAN,
                    parse_rendered_expression_ast(allocator, expr)?,
                ));
            }
        }
    }
    Some(lowered)
}

fn render_call_expression_ast(
    callee: &str,
    args: &[Argument],
    rendered_args: &[String],
    optional: bool,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let callee = parse_rendered_expression_ast(&allocator, callee)?;
    let args = render_arguments_ast(builder, &allocator, args, rendered_args)?;
    let expression = builder.expression_call(SPAN, callee, NONE, args, optional);
    Some(strip_top_level_parenthesized_expression(
        codegen_expression_with_oxc(&expression),
    ))
}

fn render_array_expression_ast(cx: &mut Context, elements: &[ArrayElement]) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut lowered = builder.vec();
    for element in elements {
        let element = match element {
            ArrayElement::Place(place) => ast::ArrayExpressionElement::from(
                parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, place))?,
            ),
            ArrayElement::Spread(place) => builder.array_expression_element_spread_element(
                SPAN,
                parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, place))?,
            ),
            ArrayElement::Hole => builder.array_expression_element_elision(SPAN),
        };
        lowered.push(element);
    }
    let expression = builder.expression_array(SPAN, lowered);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_template_literal_ast(
    cx: &mut Context,
    quasis: &[TemplateQuasi],
    subexprs: &[Place],
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut expressions = builder.vec();
    for place in subexprs {
        expressions.push(parse_rendered_expression_ast(
            &allocator,
            &codegen_place_to_expression(cx, place),
        )?);
    }
    let expression = builder.expression_template_literal(
        SPAN,
        builder.vec_from_iter(quasis.iter().enumerate().map(|(index, quasi)| {
            builder.template_element(
                SPAN,
                ast::TemplateElementValue {
                    raw: builder.atom(&quasi.raw),
                    cooked: quasi.cooked.as_deref().map(|cooked| builder.atom(cooked)),
                },
                index + 1 == quasis.len(),
                false,
            )
        })),
        expressions,
    );
    Some(codegen_expression_with_oxc(&expression))
}

fn render_meta_property_expression_ast(meta: &str, property: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let expression = builder.expression_meta_property(
        SPAN,
        builder.identifier_name(SPAN, builder.ident(meta)),
        builder.identifier_name(SPAN, builder.ident(property)),
    );
    Some(codegen_expression_with_oxc(&expression))
}

fn render_tagged_template_expression_ast(
    cx: &mut Context,
    tag: &Place,
    raw: &str,
    cooked: Option<&str>,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let tag = parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, tag))?;
    let quasi = builder.template_literal(
        SPAN,
        builder.vec1(builder.template_element(
            SPAN,
            ast::TemplateElementValue {
                raw: builder.atom(raw),
                cooked: cooked.map(|value| builder.atom(value)),
            },
            true,
            false,
        )),
        builder.vec(),
    );
    let expression = builder.expression_tagged_template(SPAN, tag, NONE, quasi);
    Some(codegen_expression_with_oxc(&expression))
}

fn render_await_expression_ast(value: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let value = parse_rendered_expression_ast(&allocator, value)?;
    let expression = builder.expression_await(SPAN, value);
    Some(codegen_expression_with_oxc(&expression))
}

fn strip_top_level_parenthesized_expression(expression: String) -> String {
    let allocator = Allocator::default();
    match parse_rendered_expression_ast(&allocator, &expression) {
        Some(ast::Expression::ParenthesizedExpression(parenthesized)) => {
            codegen_expression_with_oxc(&parenthesized.unbox().expression)
        }
        _ => expression,
    }
}

// ---- Place & identifier helpers ----

fn codegen_place_to_expression(cx: &mut Context, place: &Place) -> String {
    codegen_place_expr_value(cx, place).expr
}

fn codegen_member_object_expression(cx: &mut Context, place: &Place) -> String {
    codegen_place_expr_value(cx, place).wrap_if_needed(ExprPrecedence::Primary)
}

fn codegen_place_expr_value(cx: &mut Context, place: &Place) -> ExprValue {
    let temp_lookup = cx.temp_expr_for_place(place);
    let inline_literal = cx
        .inline_primitive_literals
        .get(&place.identifier.declaration_id)
        .cloned();
    if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() && place.identifier.name.is_none() {
        let temp_state = match &temp_lookup {
            Some(Some(ev)) => format!("mapped({})", ev.expr),
            Some(None) => "mapped(<none>)".to_string(),
            None => "missing".to_string(),
        };
        let inline_state = inline_literal.as_deref().unwrap_or("<none>");
        eprintln!(
            "[DEBUG_CODEGEN_EXPR] unnamed_place id={} decl={} loc={:?} temp={} inline_literal={}",
            place.identifier.id.0,
            place.identifier.declaration_id.0,
            place.identifier.loc,
            temp_state,
            inline_state
        );
    }
    if let Some(Some(ev)) = temp_lookup {
        return ev;
    }
    if let Some(literal) = inline_literal {
        return ExprValue::primary(literal);
    }
    if let Some(alias) = cx
        .inline_identifier_aliases
        .get(&place.identifier.declaration_id)
        .cloned()
    {
        return ExprValue::primary(alias);
    }
    // Upstream keeps compiling unnamed temporaries by materializing a temp name.
    // Do not hard-bail here; fallback naming preserves parity for optional/value-block paths.
    ExprValue::primary(identifier_name_with_cx(cx, &place.identifier))
}

fn extract_inlineable_primitive_from_place(cx: &Context, place: &Place) -> Option<String> {
    let ev = cx.temp.get(&place.identifier.declaration_id)?.as_ref()?;
    let literal = ev.expr.trim();
    if is_inlineable_primitive_literal_expression(literal) {
        Some(literal.to_string())
    } else {
        None
    }
}

fn is_inlineable_primitive_literal_expression(expr: &str) -> bool {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return false;
    }
    if matches!(trimmed, "true" | "false" | "null" | "undefined") {
        return true;
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return true;
    }
    if let Some(number) = trimmed.strip_suffix('n') {
        return !number.is_empty()
            && number
                .chars()
                .all(|c| c.is_ascii_digit() || c == '_' || c == '-' || c == '+');
    }
    trimmed.parse::<f64>().is_ok()
}

fn codegen_place_with_min_prec(
    cx: &mut Context,
    place: &Place,
    min_prec: ExprPrecedence,
) -> String {
    codegen_place_expr_value(cx, place).wrap_if_needed(min_prec)
}

fn identifier_name_with_cx(cx: &mut Context, id: &Identifier) -> String {
    if let Some(mapped) = cx.param_display_names.get(&id.declaration_id) {
        return mapped.clone();
    }
    // If this DeclarationId was already resolved, return the cached name.
    // The rename_variables pass ensures names are unique within block scopes,
    // so identifiers in non-overlapping scopes may share the same name.
    if let Some(mapped) = cx.declaration_name_overrides.get(&id.declaration_id) {
        return mapped.clone();
    }
    let base = if let Some(name) = &id.name {
        match name {
            IdentifierName::Named(n) => shift_temp_display_name(cx, id, n),
            IdentifierName::Promoted(n) => shift_temp_display_name(cx, id, n),
        }
    } else if let Some(preferred) = cx.preferred_decl_names.get(&id.declaration_id) {
        shift_temp_display_name(cx, id, preferred)
    } else {
        // Synthetic marker temporaries are minted from a high ID range (>= 900000)
        // in earlier passes. Map those to dense temp names for upstream parity.
        let generated = if id.id.0 >= 900_000 {
            let remapped_idx = if let Some(existing) = cx.temp_remap.get(&id.id) {
                *existing
            } else {
                let next = cx.next_temp_index;
                cx.next_temp_index += 1;
                cx.temp_remap.insert(id.id, next);
                next
            };
            format!("t{}", remapped_idx)
        } else {
            format!("t{}", id.id.0)
        };
        shift_temp_display_name(cx, id, &generated)
    };
    let mut emitted = base.clone();
    let retry_temp_name_conflict = cx.disable_memoization_features
        && is_codegen_temp_name(&base)
        && cx.used_declaration_names.contains(&emitted);
    let jsx_component_temp_conflict = parse_codegen_component_temp_index(&emitted).is_some()
        && cx.used_declaration_names.contains(&emitted);
    let active_block_temp_conflict = is_codegen_temp_name(&emitted)
        && cx
            .block_scope_declared_temp_names
            .iter()
            .rev()
            .any(|names| names.contains(&emitted));
    if cx.declared_names.contains(&emitted)
        || active_block_temp_conflict
        || retry_temp_name_conflict
        || jsx_component_temp_conflict
    {
        if is_codegen_temp_name(&base) {
            let next_index = parse_codegen_temp_index(&base)
                .map(|index| index.saturating_add(1))
                .unwrap_or(cx.next_temp_index);
            emitted = if id.id.0 >= 900_000 {
                fresh_synthetic_temp_name_from(cx, next_index)
            } else {
                fresh_temp_name(cx)
            };
        } else if let Some(next_index) =
            parse_codegen_component_temp_index(&base).map(|index| index.saturating_add(1))
        {
            emitted = fresh_component_temp_name_from(cx, next_index);
        } else {
            let mut suffix = 0u32;
            let mut candidate = format!("{}_{}", base, suffix);
            while cx.declared_names.contains(&candidate)
                || cx.used_declaration_names.contains(&candidate)
                || cx.reserved_child_decl_names.contains(&candidate)
            {
                suffix += 1;
                candidate = format!("{}_{}", base, suffix);
            }
            emitted = candidate;
        }
    }
    // Only dedup against reserved_child_decl_names (names from inner/child functions
    // that occupy a different scope). Skip the used_declaration_names check because
    // rename_variables already ensures block-scope-level uniqueness, and two
    // identifiers in non-overlapping block scopes are allowed to share a name.
    if cx.reserved_child_decl_names.contains(&emitted) {
        let mut suffix = 0u32;
        let mut candidate = format!("{}_{}", base, suffix);
        while cx.used_declaration_names.contains(&candidate)
            || cx.reserved_child_decl_names.contains(&candidate)
        {
            suffix += 1;
            candidate = format!("{}_{}", base, suffix);
        }
        emitted = candidate;
    }
    cx.used_declaration_names.insert(emitted.clone());
    cx.declaration_name_overrides
        .insert(id.declaration_id, emitted.clone());
    emitted
}

fn shift_temp_display_name(cx: &Context, _id: &Identifier, name: &str) -> String {
    let Some(suffix) = name.strip_prefix('t') else {
        return name.to_string();
    };
    let Ok(display_idx) = suffix.parse::<u32>() else {
        return name.to_string();
    };
    let shift = cx
        .suppressed_temp_ids
        .iter()
        .filter(|suppressed| display_idx > **suppressed)
        .count() as u32;
    let shifted = format!("t{}", display_idx.saturating_sub(shift));
    if std::env::var("DEBUG_REACTIVE_SCOPE_NAMES").is_ok() {
        eprintln!(
            "[SHIFT_TEMP] name={} display_idx={} suppressed={:?} shifted={} used_has_shifted={}",
            name,
            display_idx,
            cx.suppressed_temp_ids,
            shifted,
            cx.used_declaration_names.contains(&shifted)
        );
    }
    if shifted != name && cx.used_declaration_names.contains(&shifted) {
        // Avoid collapsing distinct temporaries onto the same display name.
        return name.to_string();
    }
    shifted
}

fn identifier_name_static(id: &Identifier) -> String {
    if let Some(ref name) = id.name {
        match name {
            IdentifierName::Named(n) => n.clone(),
            IdentifierName::Promoted(n) => n.clone(),
        }
    } else {
        format!("t{}", id.id.0)
    }
}

fn fresh_temp_name(cx: &mut Context) -> String {
    loop {
        let candidate = format!("t{}", cx.next_temp_index);
        cx.next_temp_index += 1;
        if !cx.unique_identifiers.contains(&candidate)
            && !cx.used_declaration_names.contains(&candidate)
            && !cx.declared_names.contains(&candidate)
            && !cx.reserved_child_decl_names.contains(&candidate)
        {
            cx.unique_identifiers.insert(candidate.clone());
            return candidate;
        }
    }
}

fn fresh_synthetic_temp_name_from(cx: &mut Context, start_index: u32) -> String {
    let mut next_index = start_index;
    loop {
        let candidate = format!("t{}", next_index);
        next_index += 1;
        if !cx.used_declaration_names.contains(&candidate)
            && !cx.declared_names.contains(&candidate)
            && !cx.reserved_child_decl_names.contains(&candidate)
        {
            cx.next_temp_index = cx.next_temp_index.max(next_index);
            cx.unique_identifiers.insert(candidate.clone());
            return candidate;
        }
    }
}

fn parse_codegen_component_temp_index(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix('T')?;
    if suffix.is_empty() || !suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u32>().ok()
}

fn fresh_component_temp_name_from(cx: &Context, start_index: u32) -> String {
    let mut next_index = start_index;
    loop {
        let candidate = format!("T{}", next_index);
        next_index += 1;
        if !cx.unique_identifiers.contains(&candidate)
            && !cx.used_declaration_names.contains(&candidate)
            && !cx.declared_names.contains(&candidate)
            && !cx.reserved_child_decl_names.contains(&candidate)
        {
            return candidate;
        }
    }
}

/// Truncate `ref.current` deps to just `ref` when the identifier is a useRef type.
/// Upstream: PropagateScopeDependenciesHIR.ts lines 610-620.
/// Applied at codegen as a safety net: the dep propagation pass also does this,
/// but inner function context variables may carry un-truncated deps through chains
/// where the identifier type wasn't fully resolved.
fn truncate_ref_current_dep(
    dep: &ReactiveScopeDependency,
    stable_ref_decls: &HashSet<DeclarationId>,
) -> ReactiveScopeDependency {
    if (matches!(&dep.identifier.type_, Type::Object { shape_id: Some(s) } if s == "BuiltInUseRefId")
        || stable_ref_decls.contains(&dep.identifier.declaration_id))
        && dep.path.first().is_some_and(|p| p.property == "current")
    {
        ReactiveScopeDependency {
            identifier: dep.identifier.clone(),
            path: Vec::new(),
        }
    } else {
        dep.clone()
    }
}

fn is_single_iteration_do_while_scope_block(block: &ReactiveBlock) -> bool {
    let Some(ReactiveStatement::Terminal(term_stmt)) = block.last() else {
        return false;
    };
    match &term_stmt.terminal {
        ReactiveTerminal::DoWhile { loop_block, .. } => {
            block_has_unconditional_break_terminator(loop_block)
        }
        _ => false,
    }
}

fn block_has_unconditional_break_terminator(block: &ReactiveBlock) -> bool {
    let Some(last) = block.last() else {
        return false;
    };
    if !matches!(
        last,
        ReactiveStatement::Terminal(term_stmt)
            if matches!(term_stmt.terminal, ReactiveTerminal::Break { .. })
    ) {
        return false;
    }
    for stmt in &block[..block.len() - 1] {
        if !matches!(stmt, ReactiveStatement::Instruction(_)) {
            return false;
        }
    }
    true
}

fn codegen_dependency(cx: &mut Context, dep: &ReactiveScopeDependency) -> String {
    let root_expr = if dep.path.is_empty() {
        let has_stable_named_root = dep.identifier.name.as_ref().is_some_and(|name| match name {
            IdentifierName::Named(n) | IdentifierName::Promoted(n) => !is_codegen_temp_name(n),
        });
        if has_stable_named_root {
            identifier_name_with_cx(cx, &dep.identifier)
        } else if let Some(Some(ev)) = cx.temp.get(&dep.identifier.declaration_id) {
            ev.expr.clone()
        } else {
            identifier_name_with_cx(cx, &dep.identifier)
        }
    } else {
        identifier_name_with_cx(cx, &dep.identifier)
    };
    if dep.path.is_empty() {
        return root_expr;
    }
    render_dependency_expression_ast(&root_expr, dep).unwrap_or_else(|| {
        let mut expr = root_expr;
        for path_entry in &dep.path {
            if is_valid_js_identifier_name(&path_entry.property) {
                if path_entry.optional {
                    expr = format!("{}?.{}", expr, path_entry.property);
                } else {
                    expr = format!("{}.{}", expr, path_entry.property);
                }
            } else if path_entry.property.chars().all(|c| c.is_ascii_digit()) {
                if path_entry.optional {
                    expr = format!("{}?.[{}]", expr, path_entry.property);
                } else {
                    expr = format!("{}[{}]", expr, path_entry.property);
                }
            } else {
                let escaped = path_entry
                    .property
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                if path_entry.optional {
                    expr = format!("{}?.[\"{}\"]", expr, escaped);
                } else {
                    expr = format!("{}[\"{}\"]", expr, escaped);
                }
            }
        }
        expr
    })
}

fn render_dependency_expression_ast(
    root_expr: &str,
    dep: &ReactiveScopeDependency,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut expression =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), root_expr)
            .ok()?;
    let mut has_optional = false;
    for path_entry in &dep.path {
        if is_valid_js_identifier_name(&path_entry.property) {
            expression = builder.member_expression_static(
                SPAN,
                expression,
                builder.identifier_name(SPAN, builder.atom(&path_entry.property)),
                path_entry.optional,
            ).into();
        } else if path_entry.property.chars().all(|c| c.is_ascii_digit()) {
            expression = builder.member_expression_computed(
                SPAN,
                expression,
                builder.expression_numeric_literal(
                    SPAN,
                    path_entry.property.parse::<f64>().ok()?,
                    None,
                    oxc_syntax::number::NumberBase::Decimal,
                ),
                path_entry.optional,
            ).into();
        } else {
            expression = builder.member_expression_computed(
                SPAN,
                expression,
                builder.expression_string_literal(
                    SPAN,
                    builder.atom(&path_entry.property),
                    None,
                ),
                path_entry.optional,
            ).into();
        }
        has_optional |= path_entry.optional;
    }
    if has_optional {
        expression = match expression {
            ast::Expression::ComputedMemberExpression(member) => {
                builder.expression_chain(SPAN, ast::ChainElement::ComputedMemberExpression(member))
            }
            ast::Expression::StaticMemberExpression(member) => {
                builder.expression_chain(SPAN, ast::ChainElement::StaticMemberExpression(member))
            }
            ast::Expression::PrivateFieldExpression(member) => {
                builder.expression_chain(SPAN, ast::ChainElement::PrivateFieldExpression(member))
            }
            other => other,
        };
    }
    Some(codegen_expression_with_oxc(&expression))
}

fn infer_fallback_scope_dep_exprs(cx: &mut Context, block: &ReactiveBlock) -> Vec<String> {
    let mut deps: Vec<String> = Vec::new();
    let Some(ReactiveStatement::Instruction(instr)) = block.first() else {
        return deps;
    };
    if block.len() != 1 {
        return deps;
    }

    if let InstructionValue::JsxExpression {
        props, children, ..
    } = &instr.value
    {
        for attr in props {
            let place = match attr {
                JsxAttribute::Attribute { place, .. } => place,
                JsxAttribute::SpreadAttribute { argument } => argument,
            };
            let expr = codegen_place_to_expression(cx, place);
            if should_include_fallback_scope_dep_expr(&expr) {
                deps.push(expr);
            }
        }
        if let Some(children) = children {
            for child in children {
                let expr = codegen_place_to_expression(cx, child);
                if should_include_fallback_scope_dep_expr(&expr) {
                    deps.push(expr);
                }
            }
        }
    }

    dedupe_dependency_paths(deps)
}

fn should_include_fallback_scope_dep_expr(expr: &str) -> bool {
    let trimmed = expr.trim();
    !trimmed.is_empty() && !is_inlineable_primitive_literal_expression(trimmed)
}

fn replace_dep_exprs_with_optional_fallbacks(
    selected_dep_exprs: Vec<String>,
    fallback_dep_exprs: &[String],
) -> Vec<String> {
    let selected_normalized: HashSet<String> = selected_dep_exprs
        .iter()
        .map(|expr| strip_optional_markers(expr))
        .collect();
    let fallback_ordered: Vec<String> = fallback_dep_exprs
        .iter()
        .filter(|expr| selected_normalized.contains(&strip_optional_markers(expr)))
        .cloned()
        .collect();
    let fallback_normalized: HashSet<String> = fallback_ordered
        .iter()
        .map(|expr| strip_optional_markers(expr))
        .collect();
    if !fallback_ordered.is_empty() && fallback_normalized == selected_normalized {
        return dedupe_dependency_paths(fallback_ordered);
    }

    let optional_fallbacks: HashMap<String, String> = fallback_dep_exprs
        .iter()
        .filter(|expr| expr.contains("?."))
        .map(|expr| (strip_optional_markers(expr), expr.clone()))
        .collect();
    let mut next = Vec::with_capacity(selected_dep_exprs.len());
    let mut seen = HashSet::new();
    for dep_expr in selected_dep_exprs {
        let replacement = optional_fallbacks
            .get(&strip_optional_markers(&dep_expr))
            .cloned()
            .unwrap_or(dep_expr);
        if seen.insert(replacement.clone()) {
            next.push(replacement);
        }
    }
    next
}

fn strip_optional_markers(expr: &str) -> String {
    expr.replace("?.[", "[").replace("?.", ".")
}

fn block_has_optional_computed_load_call_key(block: &ReactiveBlock) -> bool {
    let mut call_like_results: HashSet<IdentifierId> = HashSet::new();
    for stmt in block {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        if matches!(
            instr.value,
            InstructionValue::CallExpression { .. }
                | InstructionValue::MethodCall { .. }
                | InstructionValue::TaggedTemplateExpression { .. }
        ) && let Some(lvalue) = &instr.lvalue
        {
            call_like_results.insert(lvalue.identifier.id);
        }
    }
    for stmt in block {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        let InstructionValue::ComputedLoad {
            property,
            optional: true,
            ..
        } = &instr.value
        else {
            continue;
        };
        if call_like_results.contains(&property.identifier.id) {
            return true;
        }
    }
    false
}

fn widen_member_dep_expr_to_root(dep_expr: &str) -> Option<String> {
    let dep = dep_expr.trim();
    if dep.contains("?.") || dep.contains('[') {
        return None;
    }
    let dot = dep.find('.')?;
    let root = dep[..dot].trim();
    if is_valid_js_identifier_name(root) {
        Some(root.to_string())
    } else {
        None
    }
}

fn is_valid_js_identifier_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first == '$' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn compare_scope_dependency(
    a: &ReactiveScopeDependency,
    b: &ReactiveScopeDependency,
) -> std::cmp::Ordering {
    let a_name = format_dependency_name(a);
    let b_name = format_dependency_name(b);
    a_name.cmp(&b_name)
}

fn format_dependency_name(dep: &ReactiveScopeDependency) -> String {
    let mut parts = vec![identifier_name_static(&dep.identifier)];
    for entry in &dep.path {
        parts.push(format!(
            "{}{}",
            if entry.optional { "?" } else { "" },
            entry.property
        ));
    }
    parts.join(".")
}

fn sort_scope_dependencies_for_codegen(cx: &Context, deps: &mut [ReactiveScopeDependency]) {
    deps.sort_by(|a, b| {
        dependency_codegen_sort_key(cx, a).cmp(&dependency_codegen_sort_key(cx, b))
    });
}

fn sort_scope_dependency_refs_for_codegen(cx: &Context, deps: &mut [&ReactiveScopeDependency]) {
    deps.sort_by(|a, b| {
        dependency_codegen_sort_key(cx, a).cmp(&dependency_codegen_sort_key(cx, b))
    });
}

fn dependency_codegen_sort_key(cx: &Context, dep: &ReactiveScopeDependency) -> String {
    let root_name = cx
        .param_display_names
        .get(&dep.identifier.declaration_id)
        .cloned()
        .or_else(|| {
            cx.declaration_name_overrides
                .get(&dep.identifier.declaration_id)
                .cloned()
        })
        .or_else(|| {
            if dep.path.is_empty() {
                cx.temp
                    .get(&dep.identifier.declaration_id)
                    .and_then(|ev| ev.as_ref())
                    .map(|ev| ev.expr.clone())
            } else {
                None
            }
        })
        .unwrap_or_else(|| identifier_name_static(&dep.identifier));
    let mut parts = vec![root_name];
    for entry in &dep.path {
        parts.push(format!(
            "{}{}",
            if entry.optional { "?" } else { "" },
            entry.property
        ));
    }
    parts.join(".")
}

fn format_dependency_identity(dep: &ReactiveScopeDependency) -> String {
    let mut key = format!("{}", dep.identifier.declaration_id.0);
    for entry in &dep.path {
        key.push('|');
        key.push(if entry.optional { '?' } else { '.' });
        key.push_str(&entry.property);
    }
    key
}

fn maybe_replace_autodeps_with_inferred_deps(
    cx: &mut Context,
    callee_name_or_expr: &str,
    raw_args: &[Argument],
    rendered_args: &mut [String],
    callee_is_direct_name: bool,
) {
    if rendered_args.is_empty() || raw_args.is_empty() {
        return;
    }

    let hook_candidate = if callee_is_direct_name {
        callee_name_or_expr.trim().to_string()
    } else {
        // Best-effort: detect simple identifiers and member expressions.
        let raw = callee_name_or_expr.trim();
        raw.rsplit_once('.')
            .map(|(_, tail)| tail)
            .unwrap_or(raw)
            .trim_matches(|c| c == ')' || c == '(' || c == '?' || c == ' ')
            .to_string()
    };
    let hook_like = Environment::is_hook_name(&hook_candidate);
    if !hook_like {
        return;
    }
    let effect_like_hook = is_effect_like_hook_name(&hook_candidate);

    let autodeps_index = rendered_args
        .iter()
        .position(|arg| arg == "AUTODEPS" || arg.ends_with(".AUTODEPS"));
    let Some(autodeps_index) = autodeps_index else {
        if effect_like_hook {
            maybe_refine_effect_hook_dependency_array_from_callback_deps(
                cx,
                raw_args,
                rendered_args,
            );
        }
        return;
    };

    if std::env::var("DEBUG_AUTODEPS_FLOW").is_ok() {
        let raw_kinds: Vec<&'static str> = raw_args
            .iter()
            .map(|arg| match arg {
                Argument::Place(_) => "place",
                Argument::Spread(_) => "spread",
            })
            .collect();
        eprintln!(
            "[AUTODEPS_FLOW] call={} autodeps_index={} raw_kinds={:?} rendered_args={:?}",
            callee_name_or_expr, autodeps_index, raw_kinds, rendered_args
        );
    }

    // Upstream infer-effect-dependencies tracks callback captures.
    // We approximate by finding the nearest callback-like argument (scanning
    // backward from AUTODEPS) and using its captured dependency paths.
    let mut deps: Vec<String> = Vec::new();
    if autodeps_index > 0 {
        for idx in (0..autodeps_index).rev() {
            if let Argument::Place(place) = &raw_args[idx] {
                if std::env::var("DEBUG_AUTODEPS_FLOW").is_ok() {
                    eprintln!(
                        "[AUTODEPS_FLOW] probe idx={} decl={}",
                        idx, place.identifier.declaration_id.0
                    );
                }
                if let Some(found) = cx.callback_deps.get(&place.identifier.declaration_id) {
                    if std::env::var("DEBUG_AUTODEPS_FLOW").is_ok() {
                        eprintln!(
                            "[AUTODEPS_FLOW] use callback deps decl={} deps={:?}",
                            place.identifier.declaration_id.0, found
                        );
                    }
                    deps = found.clone();
                    break;
                }
            }
        }
        // Fallback: if no callback metadata, use the argument immediately before
        // AUTODEPS when it is already an explicit array literal.
        if deps.is_empty() {
            let prev = rendered_args[autodeps_index - 1].trim();
            if prev.starts_with('[') && prev.ends_with(']') {
                let inner = prev[1..prev.len() - 1].trim();
                if !inner.is_empty() {
                    deps = inner
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }
    }

    let mut deps_ordered: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for dep in deps {
        let normalized = strip_terminal_current_path(&dep).unwrap_or(dep);
        if seen.insert(normalized.clone()) {
            deps_ordered.push(normalized);
        }
    }
    rendered_args[autodeps_index] = if deps_ordered.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", deps_ordered.join(", "))
    };
}

fn is_effect_like_hook_name(name: &str) -> bool {
    matches!(
        name,
        "useEffect" | "useLayoutEffect" | "useInsertionEffect" | "useImperativeHandle"
    )
}

fn parse_rendered_array_deps(arg: &str) -> Option<Vec<String>> {
    let trimmed = arg.trim();
    if !(trimmed.starts_with('[') && trimmed.ends_with(']')) {
        return None;
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    Some(
        inner
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn maybe_refine_effect_hook_dependency_array_from_callback_deps(
    cx: &mut Context,
    raw_args: &[Argument],
    rendered_args: &mut [String],
) {
    let mut callback_idx_and_deps: Option<(usize, Vec<String>)> = None;
    for (idx, arg) in raw_args.iter().enumerate() {
        let Argument::Place(place) = arg else {
            continue;
        };
        let Some(found) = cx.callback_deps.get(&place.identifier.declaration_id) else {
            continue;
        };
        if found.is_empty() {
            continue;
        }
        callback_idx_and_deps = Some((idx, found.clone()));
        break;
    }
    let Some((callback_idx, callback_deps)) = callback_idx_and_deps else {
        return;
    };
    let mut seen_cb = HashSet::new();
    let mut normalized_callback_deps: Vec<String> = Vec::new();
    for dep in callback_deps {
        let normalized = strip_terminal_current_path(&dep).unwrap_or(dep);
        if seen_cb.insert(normalized.clone()) {
            normalized_callback_deps.push(normalized);
        }
    }
    if normalized_callback_deps.is_empty() {
        return;
    }

    let deps_array_idx = (callback_idx + 1..rendered_args.len())
        .find(|idx| parse_rendered_array_deps(&rendered_args[*idx]).is_some());
    let Some(deps_array_idx) = deps_array_idx else {
        return;
    };
    let Some(existing_deps) = parse_rendered_array_deps(&rendered_args[deps_array_idx]) else {
        return;
    };
    if existing_deps.is_empty() {
        return;
    }

    let all_cb_covered_by_existing = normalized_callback_deps.iter().all(|cb| {
        existing_deps
            .iter()
            .any(|existing| existing == cb || is_dependency_prefix(existing, cb))
    });
    if !all_cb_covered_by_existing {
        return;
    }
    let all_existing_related = existing_deps.iter().all(|existing| {
        normalized_callback_deps
            .iter()
            .any(|cb| existing == cb || is_dependency_prefix(existing, cb))
    });
    if !all_existing_related {
        return;
    }
    let has_refinement = normalized_callback_deps
        .iter()
        .any(|cb| !existing_deps.iter().any(|existing| existing == cb));
    if !has_refinement {
        return;
    }

    rendered_args[deps_array_idx] = format!("[{}]", normalized_callback_deps.join(", "));
}

/// Infer dependency expressions for a callback/function expression by tracing
/// captured context values through property-load chains in the lowered HIR.
fn infer_callback_dependency_paths(
    lowered_func: &LoweredFunction,
    stable_ref_decls: &HashSet<DeclarationId>,
    stable_setter_decls: &HashSet<DeclarationId>,
    stable_effect_event_decls: &HashSet<DeclarationId>,
    multi_source_decls: &HashSet<DeclarationId>,
    inlineable_primitive_decls: &HashMap<DeclarationId, String>,
) -> Vec<String> {
    let func = &lowered_func.func;
    let optional_sidemap =
        crate::hir::collect_optional_chain_deps::collect_optional_chain_sidemap(func);

    let mut context_roots: HashMap<DeclarationId, String> = HashMap::new();
    for place in &func.context {
        if inlineable_primitive_decls.contains_key(&place.identifier.declaration_id) {
            continue;
        }
        if let Some(name) = &place.identifier.name {
            context_roots.insert(place.identifier.declaration_id, name.value().to_string());
        }
    }
    if context_roots.is_empty() {
        return Vec::new();
    }

    let mut literal_by_id: HashMap<IdentifierId, String> = HashMap::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::Primitive { value, .. } = &instr.value {
                match value {
                    PrimitiveValue::String(s) => {
                        literal_by_id.insert(instr.lvalue.identifier.id, s.clone());
                    }
                    PrimitiveValue::Number(n) => {
                        literal_by_id.insert(instr.lvalue.identifier.id, n.to_string());
                    }
                    _ => {}
                }
            }
        }
    }

    let mut id_to_path: HashMap<IdentifierId, String> = HashMap::new();
    let mut id_to_root_decl: HashMap<IdentifierId, DeclarationId> = HashMap::new();
    // Initial seed from direct context loads.
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::LoadContext { place, .. }
                | InstructionValue::LoadLocal { place, .. } => {
                    if let Some(root) = context_roots.get(&place.identifier.declaration_id) {
                        id_to_path.insert(instr.lvalue.identifier.id, root.clone());
                        id_to_root_decl
                            .insert(instr.lvalue.identifier.id, place.identifier.declaration_id);
                    }
                }
                _ => {}
            }
        }
    }

    // Fixpoint propagation through aliases and property chains.
    loop {
        let mut changed = false;
        for (_, block) in &func.body.blocks {
            for instr in &block.instructions {
                match &instr.value {
                    InstructionValue::LoadContext { place, .. }
                    | InstructionValue::LoadLocal { place, .. } => {
                        if let Some(path) = id_to_path.get(&place.identifier.id) {
                            let next = path.clone();
                            if id_to_path.insert(instr.lvalue.identifier.id, next.clone())
                                != Some(next)
                            {
                                changed = true;
                            }
                            if let Some(root_decl) =
                                id_to_root_decl.get(&place.identifier.id).copied()
                                && id_to_root_decl.insert(instr.lvalue.identifier.id, root_decl)
                                    != Some(root_decl)
                            {
                                changed = true;
                            }
                        }
                    }
                    InstructionValue::TypeCastExpression { value, .. } => {
                        if let Some(path) = id_to_path.get(&value.identifier.id) {
                            let next = path.clone();
                            if id_to_path.insert(instr.lvalue.identifier.id, next.clone())
                                != Some(next)
                            {
                                changed = true;
                            }
                            if let Some(root_decl) =
                                id_to_root_decl.get(&value.identifier.id).copied()
                                && id_to_root_decl.insert(instr.lvalue.identifier.id, root_decl)
                                    != Some(root_decl)
                            {
                                changed = true;
                            }
                        }
                    }
                    InstructionValue::PropertyLoad {
                        object,
                        property,
                        optional,
                        ..
                    } => {
                        if let Some(base) = id_to_path.get(&object.identifier.id) {
                            let suffix = match property {
                                PropertyLiteral::String(s) => {
                                    if is_non_negative_integer_string(s) {
                                        if *optional {
                                            format!("?.[{}]", s)
                                        } else {
                                            format!("[{}]", s)
                                        }
                                    } else if is_valid_js_identifier(s) {
                                        if *optional {
                                            format!("?.{}", s)
                                        } else {
                                            format!(".{}", s)
                                        }
                                    } else if *optional {
                                        format!("?.[\"{}\"]", escape_string(s))
                                    } else {
                                        format!("[\"{}\"]", escape_string(s))
                                    }
                                }
                                PropertyLiteral::Number(n) => {
                                    if *optional {
                                        format!("?.[{}]", n)
                                    } else {
                                        format!("[{}]", n)
                                    }
                                }
                            };
                            let next = format!("{}{}", base, suffix);
                            if id_to_path.insert(instr.lvalue.identifier.id, next.clone())
                                != Some(next)
                            {
                                changed = true;
                            }
                            if let Some(root_decl) =
                                id_to_root_decl.get(&object.identifier.id).copied()
                                && id_to_root_decl.insert(instr.lvalue.identifier.id, root_decl)
                                    != Some(root_decl)
                            {
                                changed = true;
                            }
                        }
                    }
                    InstructionValue::ComputedLoad {
                        object,
                        property,
                        optional,
                        ..
                    } => {
                        if let Some(base) = id_to_path.get(&object.identifier.id)
                            && let Some(prop) = literal_by_id.get(&property.identifier.id)
                        {
                            let suffix = if is_non_negative_integer_string(prop) {
                                if *optional {
                                    format!("?.[{}]", prop)
                                } else {
                                    format!("[{}]", prop)
                                }
                            } else if is_valid_js_identifier(prop) {
                                if *optional {
                                    format!("?.{}", prop)
                                } else {
                                    format!(".{}", prop)
                                }
                            } else if *optional {
                                format!("?.[\"{}\"]", escape_string(prop))
                            } else {
                                format!("[\"{}\"]", escape_string(prop))
                            };
                            let next = format!("{}{}", base, suffix);
                            if id_to_path.insert(instr.lvalue.identifier.id, next.clone())
                                != Some(next)
                            {
                                changed = true;
                            }
                            if let Some(root_decl) =
                                id_to_root_decl.get(&object.identifier.id).copied()
                                && id_to_root_decl.insert(instr.lvalue.identifier.id, root_decl)
                                    != Some(root_decl)
                            {
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut used_paths: Vec<(String, DeclarationId)> = Vec::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            let path_builder = matches!(
                &instr.value,
                InstructionValue::LoadContext { .. }
                    | InstructionValue::LoadLocal { .. }
                    | InstructionValue::TypeCastExpression { .. }
                    | InstructionValue::PropertyLoad { .. }
                    | InstructionValue::ComputedLoad { .. }
            );
            if path_builder {
                continue;
            }
            let mut record_path = |path: String, root_decl: DeclarationId| {
                if !used_paths.iter().any(|(existing, _)| existing == &path) {
                    used_paths.push((path, root_decl));
                }
            };

            visitors::for_each_instruction_operand(instr, |place| {
                if let Some(optional_dep) = optional_sidemap
                    .temporaries_read_in_optional
                    .get(&place.identifier.id)
                {
                    let optional_path = callback_scope_dependency_to_path(optional_dep);
                    record_path(optional_path, optional_dep.identifier.declaration_id);
                    return;
                }
                if let (Some(path), Some(root_decl)) = (
                    id_to_path.get(&place.identifier.id),
                    id_to_root_decl.get(&place.identifier.id),
                ) {
                    record_path(path.clone(), *root_decl);
                }
            });
        }
        visitors::for_each_terminal_operand(&block.terminal, |place| {
            if let Some(optional_dep) = optional_sidemap
                .temporaries_read_in_optional
                .get(&place.identifier.id)
            {
                let optional_path = callback_scope_dependency_to_path(optional_dep);
                if !used_paths
                    .iter()
                    .any(|(existing, _)| existing == &optional_path)
                {
                    used_paths.push((optional_path, optional_dep.identifier.declaration_id));
                }
                return;
            }
            if let (Some(path), Some(root_decl)) = (
                id_to_path.get(&place.identifier.id),
                id_to_root_decl.get(&place.identifier.id),
            ) && !used_paths.iter().any(|(existing, _)| existing == path)
            {
                used_paths.push((path.clone(), *root_decl));
            }
        });
    }

    if std::env::var("DEBUG_INFER_CALLBACK_DEPS").is_ok() {
        let mut dbg_paths: Vec<String> = used_paths
            .iter()
            .map(|(p, d)| format!("{}@{}", p, d.0))
            .collect();
        dbg_paths.sort();
        eprintln!("[INFER_CALLBACK_DEPS] raw={:?}", dbg_paths);
    }

    let is_direct_root_path =
        |path: &str| !path.contains('.') && !path.contains('?') && !path.contains('[');
    let mut explicit_root_paths: Vec<(String, DeclarationId)> = Vec::new();
    for (path, root_decl) in &used_paths {
        if !is_direct_root_path(path) {
            continue;
        }
        if stable_ref_decls.contains(root_decl) {
            continue;
        }
        if stable_setter_decls.contains(root_decl) && !multi_source_decls.contains(root_decl) {
            continue;
        }
        if stable_effect_event_decls.contains(root_decl) && !multi_source_decls.contains(root_decl)
        {
            continue;
        }
        if explicit_root_paths
            .iter()
            .any(|(existing, decl)| existing == path && *decl == *root_decl)
        {
            continue;
        }
        explicit_root_paths.push((path.clone(), *root_decl));
    }

    let mut deduped = dedupe_dependency_paths_with_roots(used_paths);

    // Preserve non-ref `.current` for callback guard precision.
    // For stable local refs, normalize `x.current` -> `x`.
    let mut normalized: Vec<(String, DeclarationId)> = Vec::new();
    for (path, root_decl) in deduped.drain(..) {
        // Stable refs should not contribute callback invalidation deps.
        // Keep parity with upstream behavior for useRef captures under
        // preserve-memoization guarantees.
        if stable_ref_decls.contains(&root_decl) {
            continue;
        }
        if let Some(stripped) = strip_terminal_current_path(&path) {
            let stripped_is_direct_root =
                !stripped.contains('.') && !stripped.contains('?') && !stripped.contains('[');
            if stripped_is_direct_root {
                if !stripped.is_empty() {
                    normalized.push((stripped, root_decl));
                }
            } else {
                normalized.push((path, root_decl));
            }
        } else {
            normalized.push((path, root_decl));
        }
    }

    // Omit direct stable setters unless they are control-flow merged.
    normalized.retain(|(path, root_decl)| {
        if stable_setter_decls.contains(root_decl) && !multi_source_decls.contains(root_decl) {
            let is_direct_root = !path.contains('.') && !path.contains('?') && !path.contains('[');
            return !is_direct_root;
        }
        if stable_effect_event_decls.contains(root_decl) && !multi_source_decls.contains(root_decl)
        {
            let is_direct_root = !path.contains('.') && !path.contains('?') && !path.contains('[');
            return !is_direct_root;
        }
        true
    });

    let mut final_pairs = dedupe_dependency_paths_with_roots(normalized);
    for (root_path, root_decl) in explicit_root_paths {
        // Preserve explicit direct-root reads only when no surviving child
        // dependency for the same declaration remains. Otherwise we would
        // reintroduce an over-broad root (e.g. `bar`) and collapse refined
        // callback deps (`bar.baz`, `bar.qux`) back to that root.
        let has_surviving_child_for_root = final_pairs.iter().any(|(existing, decl)| {
            *decl == root_decl
                && existing != &root_path
                && is_dependency_prefix(&root_path, existing)
        });
        if has_surviving_child_for_root {
            continue;
        }
        if final_pairs
            .iter()
            .any(|(existing, decl)| *decl == root_decl && existing == &root_path)
        {
            continue;
        }
        final_pairs.push((root_path, root_decl));
    }
    if std::env::var("DEBUG_INFER_CALLBACK_DEPS").is_ok() {
        let mut dbg_paths: Vec<String> = final_pairs
            .iter()
            .map(|(p, d)| format!("{}@{}", p, d.0))
            .collect();
        dbg_paths.sort();
        eprintln!("[INFER_CALLBACK_DEPS] final={:?}", dbg_paths);
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered = Vec::new();
    for (path, _) in final_pairs {
        let normalized = normalize_root_optional_dependency(&path);
        if seen.insert(normalized.clone()) {
            ordered.push(normalized);
        }
    }

    ordered
}

fn callback_scope_dependency_to_path(dep: &ReactiveScopeDependency) -> String {
    let mut expr = identifier_name_static(&dep.identifier);
    for entry in &dep.path {
        if is_valid_js_identifier(&entry.property) {
            if entry.optional {
                expr = format!("{}?.{}", expr, entry.property);
            } else {
                expr = format!("{}.{}", expr, entry.property);
            }
        } else if is_non_negative_integer_string(&entry.property) {
            if entry.optional {
                expr = format!("{}?.[{}]", expr, entry.property);
            } else {
                expr = format!("{}[{}]", expr, entry.property);
            }
        } else if entry.optional {
            expr = format!("{}?.[\"{}\"]", expr, escape_string(&entry.property));
        } else {
            expr = format!("{}[\"{}\"]", expr, escape_string(&entry.property));
        }
    }
    expr
}

fn collect_callback_deps_from_reactive_block(
    block: &ReactiveBlock,
    stable_ref_decls: &HashSet<DeclarationId>,
    stable_setter_decls: &HashSet<DeclarationId>,
    stable_effect_event_decls: &HashSet<DeclarationId>,
    multi_source_decls: &HashSet<DeclarationId>,
    inlineable_primitive_decls: &HashMap<DeclarationId, String>,
    out: &mut HashMap<DeclarationId, Vec<String>>,
) {
    for stmt in block {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(lvalue) = &instr.lvalue {
                    match &instr.value {
                        InstructionValue::FunctionExpression { lowered_func, .. }
                        | InstructionValue::ObjectMethod { lowered_func, .. } => {
                            out.entry(lvalue.identifier.declaration_id)
                                .or_insert_with(|| {
                                    infer_callback_dependency_paths(
                                        lowered_func,
                                        stable_ref_decls,
                                        stable_setter_decls,
                                        stable_effect_event_decls,
                                        multi_source_decls,
                                        inlineable_primitive_decls,
                                    )
                                });
                        }
                        _ => {}
                    }
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                collect_callback_deps_from_reactive_block(
                    &scope_block.instructions,
                    stable_ref_decls,
                    stable_setter_decls,
                    stable_effect_event_decls,
                    multi_source_decls,
                    inlineable_primitive_decls,
                    out,
                );
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                collect_callback_deps_from_reactive_block(
                    &scope_block.instructions,
                    stable_ref_decls,
                    stable_setter_decls,
                    stable_effect_event_decls,
                    multi_source_decls,
                    inlineable_primitive_decls,
                    out,
                );
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_callback_deps_from_reactive_terminal(
                    &term_stmt.terminal,
                    stable_ref_decls,
                    stable_setter_decls,
                    stable_effect_event_decls,
                    multi_source_decls,
                    inlineable_primitive_decls,
                    out,
                );
            }
        }
    }
}

fn collect_callback_deps_from_reactive_terminal(
    terminal: &ReactiveTerminal,
    stable_ref_decls: &HashSet<DeclarationId>,
    stable_setter_decls: &HashSet<DeclarationId>,
    stable_effect_event_decls: &HashSet<DeclarationId>,
    multi_source_decls: &HashSet<DeclarationId>,
    inlineable_primitive_decls: &HashMap<DeclarationId, String>,
    out: &mut HashMap<DeclarationId, Vec<String>>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_callback_deps_from_reactive_block(
                consequent,
                stable_ref_decls,
                stable_setter_decls,
                stable_effect_event_decls,
                multi_source_decls,
                inlineable_primitive_decls,
                out,
            );
            if let Some(alt) = alternate {
                collect_callback_deps_from_reactive_block(
                    alt,
                    stable_ref_decls,
                    stable_setter_decls,
                    stable_effect_event_decls,
                    multi_source_decls,
                    inlineable_primitive_decls,
                    out,
                );
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_callback_deps_from_reactive_block(
                        block,
                        stable_ref_decls,
                        stable_setter_decls,
                        stable_effect_event_decls,
                        multi_source_decls,
                        inlineable_primitive_decls,
                        out,
                    );
                }
            }
        }
        ReactiveTerminal::For { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_callback_deps_from_reactive_block(
                loop_block,
                stable_ref_decls,
                stable_setter_decls,
                stable_effect_event_decls,
                multi_source_decls,
                inlineable_primitive_decls,
                out,
            );
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_callback_deps_from_reactive_block(
                block,
                stable_ref_decls,
                stable_setter_decls,
                stable_effect_event_decls,
                multi_source_decls,
                inlineable_primitive_decls,
                out,
            );
            collect_callback_deps_from_reactive_block(
                handler,
                stable_ref_decls,
                stable_setter_decls,
                stable_effect_event_decls,
                multi_source_decls,
                inlineable_primitive_decls,
                out,
            );
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

fn resolve_place_name(cx: &Context, place: &Place) -> Option<String> {
    if let Some(name) = cx.resolved_names.get(&place.identifier.id) {
        return Some(name.clone());
    }
    place
        .identifier
        .name
        .as_ref()
        .map(|name| name.value().to_string())
}

fn is_use_ref_name(name: &str) -> bool {
    let candidate = normalize_hook_candidate(name);
    candidate == "useRef"
}

fn collect_local_declaration_names_from_lowered_function(
    lowered_func: &LoweredFunction,
) -> HashSet<String> {
    let mut names: HashSet<String> = HashSet::new();
    for param in &lowered_func.func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        if let Some(name) = &place.identifier.name {
            names.insert(name.value().to_string());
        }
    }
    for (_, block) in &lowered_func.func.body.blocks {
        for instr in &block.instructions {
            match &instr.value {
                InstructionValue::StoreLocal { lvalue, .. } => {
                    if lvalue.kind != InstructionKind::Reassign
                        && let Some(name) = &lvalue.place.identifier.name
                    {
                        names.insert(name.value().to_string());
                    }
                }
                InstructionValue::DeclareLocal { lvalue, .. } => {
                    if let Some(name) = &lvalue.place.identifier.name {
                        names.insert(name.value().to_string());
                    }
                }
                _ => {}
            }
        }
    }
    names
}

fn extract_hook_name(name: &str) -> Option<&str> {
    let candidate = normalize_hook_candidate(name);
    if Environment::is_hook_name(candidate) {
        Some(candidate)
    } else {
        None
    }
}

fn normalize_hook_candidate(name: &str) -> &str {
    let raw = name.trim();
    let tail = raw.rsplit_once('.').map_or(raw, |(_, tail)| tail);
    let tail = tail.rsplit_once('$').map_or(tail, |(_, tail)| tail);
    tail.trim_matches(|c| c == ')' || c == '(' || c == '?' || c == ' ')
}

fn load_global_resolved_name(binding: &NonLocalBinding) -> String {
    match binding {
        NonLocalBinding::ImportSpecifier { imported, .. } => imported.clone(),
        _ => binding.name().to_string(),
    }
}

fn is_stable_setter_hook_element(hook_name: &str, index: usize) -> bool {
    match hook_name {
        "useState" | "useReducer" | "useActionState" => index == 1,
        _ => false,
    }
}

fn is_stable_setter_hook_result_type(id: &Identifier) -> bool {
    matches!(
        &id.type_,
        Type::Object {
            shape_id: Some(shape_id),
        } if matches!(
            shape_id.as_str(),
            "BuiltInUseState"
                | "BuiltInUseReducer"
                | "BuiltInUseActionState"
                | "BuiltInUseStateHookResult"
                | "BuiltInUseReducerHookResult"
                | "BuiltInUseActionStateHookResult"
        )
    )
}

fn is_dependency_prefix(prefix: &str, path: &str) -> bool {
    if prefix == path {
        return true;
    }
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.starts_with('.') || rest.starts_with("?.") || rest.starts_with('[')
}

fn is_current_descendant_dependency(prefix: &str, path: &str) -> bool {
    if !is_dependency_prefix(prefix, path) || prefix == path {
        return false;
    }
    let mut rest = &path[prefix.len()..];
    if let Some(stripped) = rest.strip_prefix("?.") {
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix('.') {
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let key = stripped[..end].trim_matches('"').trim_matches('\'');
            return key == "current";
        }
        return false;
    } else {
        return false;
    }

    if !rest.starts_with("current") {
        return false;
    }
    match rest.as_bytes().get("current".len()) {
        None => true,
        Some(next) => matches!(next, b'.' | b'?' | b'['),
    }
}

fn choose_callback_dep_override(
    scope_dep_exprs: &[String],
    callback_dep_exprs: &[String],
) -> Vec<String> {
    if scope_dep_exprs.is_empty() && !callback_dep_exprs.is_empty() {
        let mut callback_sorted: Vec<String> = callback_dep_exprs.to_vec();
        callback_sorted.sort();
        callback_sorted.dedup();
        let all_property_paths = callback_sorted
            .iter()
            .all(|dep| dep.contains('.') || dep.contains("?.") || dep.contains('['));
        if all_property_paths {
            return callback_sorted;
        }
    }

    let mut resolved: Vec<String> = Vec::new();
    let mut changed = false;

    for scope_dep in scope_dep_exprs {
        let mut exact: Option<&str> = None;
        let mut child_refinements: Vec<&str> = Vec::new();
        let mut parent_generalization: Option<&str> = None;

        for cb_dep in callback_dep_exprs {
            let cb = cb_dep.as_str();
            if cb == scope_dep {
                exact = Some(cb);
                break;
            }
            if is_dependency_prefix(scope_dep, cb) {
                child_refinements.push(cb);
            } else if is_dependency_prefix(cb, scope_dep) {
                // Prefer the closest generalization (longest prefix).
                if parent_generalization.is_none_or(|best| cb.len() > best.len()) {
                    parent_generalization = Some(cb);
                }
            }
        }

        if let Some(exact) = exact {
            if exact != scope_dep {
                changed = true;
            }
            resolved.push(exact.to_string());
            continue;
        }

        if !child_refinements.is_empty() {
            if child_refinements
                .iter()
                .all(|refined| is_current_descendant_dependency(scope_dep, refined))
            {
                resolved.push(scope_dep.clone());
                continue;
            }
            changed = true;
            child_refinements.sort_unstable();
            child_refinements.dedup();
            for refined in child_refinements {
                resolved.push(refined.to_string());
            }
            continue;
        }

        if let Some(parent) = parent_generalization {
            // Allow a narrow upstream-aligned generalization for terminal
            // `.current` paths: prefer `ref` over `ref.current`.
            if strip_terminal_current_path(scope_dep)
                .as_deref()
                .is_some_and(|stripped| stripped == parent)
            {
                changed = true;
                resolved.push(parent.to_string());
                continue;
            }
        }

        // For all other parent-generalization cases, keep scope deps unchanged.
        return Vec::new();
    }

    resolved.sort();
    resolved.dedup();
    if changed {
        return resolved;
    }

    // If callback captures are a strict superset of scope deps (all exact matches
    // for the scope deps, plus additional callback deps), prefer callback deps.
    let mut callback_sorted: Vec<String> = callback_dep_exprs.to_vec();
    callback_sorted.sort();
    callback_sorted.dedup();
    let all_scope_exact = scope_dep_exprs
        .iter()
        .all(|scope_dep| callback_sorted.iter().any(|cb| cb == scope_dep));
    let has_extra = callback_sorted
        .iter()
        .any(|cb| !scope_dep_exprs.iter().any(|scope_dep| scope_dep == cb));
    if all_scope_exact && has_extra {
        let scope_roots: HashSet<&str> = scope_dep_exprs
            .iter()
            .map(|dep| dependency_root_name(dep))
            .collect();
        let extras_related_to_member_scope_dep = callback_sorted
            .iter()
            .filter(|cb| !scope_dep_exprs.iter().any(|scope_dep| scope_dep == *cb))
            .all(|extra| {
                let root = dependency_root_name(extra);
                if !scope_roots.contains(root) {
                    return false;
                }
                scope_dep_exprs.iter().any(|scope_dep| {
                    dependency_root_name(scope_dep) == root
                        && (scope_dep.contains('.')
                            || scope_dep.contains("?.")
                            || scope_dep.contains('['))
                })
            });
        if extras_related_to_member_scope_dep {
            return callback_sorted;
        }
    }
    Vec::new()
}

fn dedupe_dependency_paths(mut paths: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));

    let mut deduped: Vec<String> = Vec::new();
    for path in paths {
        let has_prefix = deduped
            .iter()
            .any(|existing| is_dependency_prefix(existing, &path));
        if has_prefix {
            continue;
        }
        deduped.retain(|existing| !is_dependency_prefix(&path, existing));
        deduped.push(path);
    }
    deduped
}

fn dedupe_dependency_paths_with_roots(
    paths: Vec<(String, DeclarationId)>,
) -> Vec<(String, DeclarationId)> {
    let mut unique: Vec<(String, DeclarationId)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (path, root_decl) in paths {
        if seen.insert(path.clone()) {
            unique.push((path, root_decl));
        }
    }

    // Optional/member callback deps should not be collapsed to an over-broad
    // parent root when sibling/optional child paths are present.
    let mut filtered: Vec<(String, DeclarationId)> = Vec::new();
    for (path, root_decl) in &unique {
        let mut child_keys: HashSet<String> = HashSet::new();
        let mut has_optional_child = false;
        for (other, _) in &unique {
            if other == path || !is_dependency_prefix(path, other) {
                continue;
            }
            if other.contains("?.") {
                has_optional_child = true;
            }
            if let Some(key) = immediate_child_dependency_key(path, other) {
                child_keys.insert(key);
            }
        }
        let should_drop_parent = has_optional_child || child_keys.len() > 1;
        if !should_drop_parent {
            filtered.push((path.clone(), *root_decl));
        }
    }

    let mut deduped: Vec<(String, DeclarationId)> = Vec::new();
    for (path, root_decl) in filtered {
        let has_prefix = deduped
            .iter()
            .any(|(existing, _)| is_dependency_prefix(existing, &path));
        if has_prefix {
            continue;
        }
        deduped.retain(|(existing, _)| !is_dependency_prefix(&path, existing));
        deduped.push((path, root_decl));
    }
    deduped
}

fn immediate_child_dependency_key(parent: &str, child: &str) -> Option<String> {
    if parent == child || !is_dependency_prefix(parent, child) {
        return None;
    }
    let mut rest = &child[parent.len()..];
    let mut optional = false;
    if let Some(stripped) = rest.strip_prefix("?.") {
        optional = true;
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix('.') {
        rest = stripped;
    } else if let Some(stripped) = rest.strip_prefix('[') {
        if let Some(end) = stripped.find(']') {
            let segment = &stripped[..=end];
            return Some(format!("[{}]", segment));
        }
        return Some("[?]".to_string());
    } else {
        return None;
    }
    if rest.is_empty() {
        return None;
    }
    let segment_end = rest.find(['.', '?', '[']).unwrap_or(rest.len());
    let segment = &rest[..segment_end];
    if segment.is_empty() {
        None
    } else if optional {
        Some(format!("?.{}", segment))
    } else {
        Some(format!(".{}", segment))
    }
}

fn normalize_root_optional_dependency(path: &str) -> String {
    if let Some((prefix, suffix)) = path.split_once("?.")
        && !prefix.contains('.')
        && !prefix.contains('[')
        && !prefix.contains('?')
        && is_valid_js_identifier(suffix)
    {
        return format!("{}.{}", prefix, suffix);
    }
    path.to_string()
}

fn strip_terminal_current_path(path: &str) -> Option<String> {
    if let Some(base) = path.strip_suffix(".current") {
        return Some(base.to_string());
    }
    if let Some(base) = path.strip_suffix("?.current") {
        return Some(base.to_string());
    }
    None
}

fn dependency_root_name(path: &str) -> &str {
    let mut end = path.len();
    for (idx, ch) in path.char_indices() {
        if ch == '.' || ch == '?' || ch == '[' {
            end = idx;
            break;
        }
    }
    &path[..end]
}

fn is_stable_setter_identifier(id: &Identifier) -> bool {
    let shape = match &id.type_ {
        Type::Object {
            shape_id: Some(shape),
        } => Some(shape.as_str()),
        Type::Function {
            shape_id: Some(shape),
            ..
        } => Some(shape.as_str()),
        _ => None,
    };
    matches!(
        shape,
        Some("BuiltInSetState" | "BuiltInSetActionState" | "BuiltInDispatch")
    )
}

// ---- Pattern codegen ----

fn codegen_pattern(cx: &mut Context, pattern: &Pattern) -> String {
    render_pattern_with_oxc(cx, pattern).unwrap_or_else(|| match pattern {
        Pattern::Array(arr) => {
            let items: Vec<String> = arr
                .items
                .iter()
                .map(|item| match item {
                    ArrayElement::Place(p) => identifier_name_with_cx(cx, &p.identifier),
                    ArrayElement::Spread(p) => {
                        format!("...{}", identifier_name_with_cx(cx, &p.identifier))
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
                .map(|prop| match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        let key = codegen_object_property_key_str(&p.key);
                        let val = identifier_name_with_cx(cx, &p.place.identifier);
                        let can_shorthand =
                            key == val && matches!(&p.key, ObjectPropertyKey::Identifier(_));
                        if can_shorthand {
                            key
                        } else {
                            format!("{}: {}", key, val)
                        }
                    }
                    ObjectPropertyOrSpread::Spread(p) => {
                        format!("...{}", identifier_name_with_cx(cx, &p.identifier))
                    }
                })
                .collect();
            format!("{{{}}}", props.join(", "))
        }
    })
}

fn render_pattern_with_oxc(cx: &mut Context, pattern: &Pattern) -> Option<String> {
    const PLACEHOLDER: &str = "__codex_pattern_rhs";
    let rendered = render_reactive_destructure_statement_ast(
        cx,
        pattern,
        PLACEHOLDER,
        Some(ast::VariableDeclarationKind::Let),
    )?;
    let trimmed = rendered.trim();
    let body = trimmed.strip_prefix("let ")?;
    let (lhs, rhs) = body.split_once(" = ")?;
    if rhs.trim_end_matches(';').trim() != PLACEHOLDER {
        return None;
    }
    Some(lhs.trim().to_string())
}

fn pattern_operands(pattern: &Pattern) -> Vec<&Place> {
    let mut result = Vec::new();
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayElement::Place(p) | ArrayElement::Spread(p) => result.push(p),
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => result.push(&p.place),
                    ObjectPropertyOrSpread::Spread(p) => result.push(p),
                }
            }
        }
    }
    result
}

fn try_build_reactive_binding_pattern_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    cx: &mut Context,
    pattern: &Pattern,
) -> Option<ast::BindingPattern<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        elements.push(Some(
                            builder.binding_pattern_binding_identifier(SPAN, builder.ident(&name)),
                        ));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        rest = Some(builder.alloc_binding_rest_element(
                            SPAN,
                            builder.binding_pattern_binding_identifier(SPAN, builder.ident(&name)),
                        ));
                    }
                    ArrayElement::Hole => {
                        if rest.is_some() {
                            return None;
                        }
                        elements.push(None);
                    }
                }
            }
            Some(builder.binding_pattern_array_pattern(SPAN, elements, rest))
        }
        Pattern::Object(obj) => {
            let mut properties = builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        let target_name = identifier_name_with_cx(cx, &property.place.identifier);
                        let computed_key_source = match &property.key {
                            ObjectPropertyKey::Computed(place) => {
                                Some(codegen_place_to_expression(cx, place))
                            }
                            _ => None,
                        };
                        let (key, computed) = make_object_property_key_ast(
                            builder,
                            allocator,
                            &property.key,
                            computed_key_source.as_deref(),
                        )?;
                        let shorthand = matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        );
                        properties.push(builder.binding_property(
                            SPAN,
                            key,
                            builder.binding_pattern_binding_identifier(
                                SPAN,
                                builder.ident(&target_name),
                            ),
                            shorthand,
                            computed,
                        ));
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if rest.is_some() || index + 1 != obj.properties.len() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        rest = Some(builder.alloc_binding_rest_element(
                            SPAN,
                            builder.binding_pattern_binding_identifier(SPAN, builder.ident(&name)),
                        ));
                    }
                }
            }
            Some(builder.binding_pattern_object_pattern(SPAN, properties, rest))
        }
    }
}

fn try_build_reactive_assignment_target_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    cx: &mut Context,
    pattern: &Pattern,
) -> Option<ast::AssignmentTarget<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        elements.push(Some(ast::AssignmentTargetMaybeDefault::from(
                            ast::AssignmentTarget::from(
                                builder.simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    builder.ident(&name),
                                ),
                            ),
                        )));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        rest = Some(builder.alloc_assignment_target_rest(
                            SPAN,
                            ast::AssignmentTarget::from(
                                builder.simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    builder.ident(&name),
                                ),
                            ),
                        ));
                    }
                    ArrayElement::Hole => {
                        if rest.is_some() {
                            return None;
                        }
                        elements.push(None);
                    }
                }
            }
            Some(ast::AssignmentTarget::from(
                builder.assignment_target_pattern_array_assignment_target(SPAN, elements, rest),
            ))
        }
        Pattern::Object(obj) => {
            let mut properties = builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        let target_name = identifier_name_with_cx(cx, &property.place.identifier);
                        if matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        ) {
                            properties.push(
                                builder
                                    .assignment_target_property_assignment_target_property_identifier(
                                        SPAN,
                                        builder.identifier_reference(
                                            SPAN,
                                            builder.ident(&target_name),
                                        ),
                                        None,
                                    ),
                            );
                            continue;
                        }
                        let computed_key_source = match &property.key {
                            ObjectPropertyKey::Computed(place) => {
                                Some(codegen_place_to_expression(cx, place))
                            }
                            _ => None,
                        };
                        let (key, computed) = make_object_property_key_ast(
                            builder,
                            allocator,
                            &property.key,
                            computed_key_source.as_deref(),
                        )?;
                        properties.push(
                            builder.assignment_target_property_assignment_target_property_property(
                                SPAN,
                                key,
                                ast::AssignmentTargetMaybeDefault::from(
                                    ast::AssignmentTarget::from(
                                        builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                builder.ident(&target_name),
                                            ),
                                    ),
                                ),
                                computed,
                            ),
                        );
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if rest.is_some() || index + 1 != obj.properties.len() {
                            return None;
                        }
                        let name = identifier_name_with_cx(cx, &place.identifier);
                        rest = Some(builder.alloc_assignment_target_rest(
                            SPAN,
                            ast::AssignmentTarget::from(
                                builder.simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    builder.ident(&name),
                                ),
                            ),
                        ));
                    }
                }
            }
            Some(ast::AssignmentTarget::from(
                builder.assignment_target_pattern_object_assignment_target(SPAN, properties, rest),
            ))
        }
    }
}

fn render_reactive_destructure_statement_ast(
    cx: &mut Context,
    pattern: &Pattern,
    rhs: &str,
    declaration_kind: Option<ast::VariableDeclarationKind>,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let rhs_expression =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), rhs).ok()?;
    let statement = if let Some(kind) = declaration_kind {
        let pattern = try_build_reactive_binding_pattern_ast(builder, &allocator, cx, pattern)?;
        ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
            SPAN,
            kind,
            builder.vec1(builder.variable_declarator(
                SPAN,
                kind,
                pattern,
                NONE,
                Some(rhs_expression),
                false,
            )),
            false,
        ))
    } else {
        let target = try_build_reactive_assignment_target_ast(builder, &allocator, cx, pattern)?;
        builder.statement_expression(
            SPAN,
            builder.expression_assignment(SPAN, AssignmentOperator::Assign, target, rhs_expression),
        )
    };
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

/// Upstream lowers destructuring declarations to mutable context vars through a
/// temporary destructure followed by explicit assignments (e.g. `let [t0] = v; x = t0;`).
/// Our lowered HIR may still carry `Destructure(Reassign)` directly into captured
/// mutable context declarations; bridge that shape in codegen for parity.
fn maybe_codegen_captured_context_destructure_bridge(
    cx: &mut Context,
    pattern: &Pattern,
    rhs: &str,
    all_declared: bool,
) -> Option<String> {
    if !all_declared {
        return None;
    }
    let mutable_captured = cx.mutable_captured_in_child_functions.clone();
    let needs_bridge = |decl: DeclarationId| mutable_captured.contains(&decl);

    match pattern {
        Pattern::Array(arr) => {
            if arr.items.is_empty() {
                return None;
            }
            let mut targets: Vec<&Place> = Vec::new();
            for item in &arr.items {
                let ArrayElement::Place(place) = item else {
                    return None;
                };
                if !needs_bridge(place.identifier.declaration_id) {
                    return None;
                }
                targets.push(place);
            }
            if targets.is_empty() {
                return None;
            }

            let allocator = Allocator::default();
            let builder = AstBuilder::new(&allocator);
            let rhs_expr =
                parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), rhs)
                    .ok()?;
            let temp_names = (0..targets.len())
                .map(|idx| format!("t{}", idx))
                .collect::<Vec<_>>();
            let elements = builder.vec_from_iter(temp_names.iter().map(|name| {
                Some(builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)))
            }));
            let array_pattern = builder.binding_pattern_array_pattern(SPAN, elements, NONE);
            let mut statements = vec![ast::Statement::VariableDeclaration(
                builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    builder.vec1(builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        array_pattern,
                        NONE,
                        Some(rhs_expr),
                        false,
                    )),
                    false,
                ),
            )];
            for (target, temp_name) in targets.iter().zip(temp_names.iter()) {
                statements.push(build_identifier_assignment_statement_ast(
                    builder,
                    &identifier_name_with_cx(cx, &target.identifier),
                    temp_name,
                ));
            }
            Some(codegen_statements_with_oxc(&statements))
        }
        Pattern::Object(obj) => {
            if obj.properties.is_empty() {
                return None;
            }
            let allocator = Allocator::default();
            let builder = AstBuilder::new(&allocator);
            let rhs_expr =
                parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), rhs)
                    .ok()?;
            let mut properties = builder.vec();
            let mut assignments = Vec::new();

            for (idx, prop) in obj.properties.iter().enumerate() {
                let ObjectPropertyOrSpread::Property(property) = prop else {
                    return None;
                };
                if property.type_ != ObjectPropertyType::Property {
                    return None;
                }
                if !needs_bridge(property.place.identifier.declaration_id) {
                    return None;
                }

                let temp_name = format!("t{}", idx);
                let computed_key_source = match &property.key {
                    ObjectPropertyKey::Computed(place) => {
                        Some(codegen_place_to_expression(cx, place))
                    }
                    _ => None,
                };
                let (key, computed) = make_object_property_key_ast(
                    builder,
                    &allocator,
                    &property.key,
                    computed_key_source.as_deref(),
                )?;
                properties.push(builder.binding_property(
                    SPAN,
                    key,
                    builder.binding_pattern_binding_identifier(SPAN, builder.ident(&temp_name)),
                    false,
                    computed,
                ));
                assignments.push(build_identifier_assignment_statement_ast(
                    builder,
                    &identifier_name_with_cx(cx, &property.place.identifier),
                    &temp_name,
                ));
            }

            let object_pattern = builder.binding_pattern_object_pattern(SPAN, properties, NONE);
            let mut statements = vec![ast::Statement::VariableDeclaration(
                builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    builder.vec1(builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        object_pattern,
                        NONE,
                        Some(rhs_expr),
                        false,
                    )),
                    false,
                ),
            )];
            statements.extend(assignments);
            Some(codegen_statements_with_oxc(&statements))
        }
    }
}

// ---- Object property codegen ----

fn codegen_object_property(cx: &mut Context, prop: &ObjectPropertyOrSpread) -> String {
    match prop {
        ObjectPropertyOrSpread::Property(p) => {
            let key = codegen_object_property_key(cx, &p.key);
            match &p.type_ {
                ObjectPropertyType::Property => {
                    let val = codegen_place_to_expression(cx, &p.place);
                    // Shorthand check
                    if key == val && !matches!(&p.key, ObjectPropertyKey::Computed(_)) {
                        key
                    } else if matches!(&p.key, ObjectPropertyKey::Computed(_)) {
                        format!("[{}]: {}", key, val)
                    } else {
                        format!("{}: {}", key, val)
                    }
                }
                ObjectPropertyType::Method => {
                    let computed_key_source = match &p.key {
                        ObjectPropertyKey::Computed(place) => {
                            Some(codegen_place_to_expression(cx, place))
                        }
                        _ => None,
                    };
                    if let Some(&idx) = cx.object_methods.get(&p.place.identifier.id) {
                        let lf = cx.object_methods_store[idx].lowered_func.clone();
                        // Build inner function body using reactive codegen
                        let inner_hir = lf.func.clone();
                        let mut reactive_func =
                            super::build_reactive_function::build_reactive_function(inner_hir);
                        super::prune_unused_labels_reactive::prune_unused_labels(
                            &mut reactive_func,
                        );
                        super::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_func);
                        let _ = super::prune_hoisted_contexts::prune_hoisted_contexts(
                            &mut reactive_func,
                        );
                        let mut inner_result =
                            codegen_reactive_function_with_options_and_fbt_operands(
                                &reactive_func,
                                cx.unique_identifiers.clone(),
                                CodegenReactiveOptions {
                                    enable_name_anonymous_functions: cx
                                        .enable_name_anonymous_functions,
                                    ..CodegenReactiveOptions::default()
                                },
                                cx.fbt_operands.clone(),
                            );
                        adopt_codegen_error(cx, inner_result.error.take());
                        let body_trimmed = inner_result.body.trim();
                        if let Some(rendered) = render_object_method_ast(
                            &p.key,
                            computed_key_source.as_deref(),
                            &lf.func.params,
                            &inner_result.param_names,
                            body_trimmed,
                            &lf.func.directives,
                            lf.func.async_,
                            lf.func.generator,
                        ) {
                            rendered
                        } else {
                            cx.codegen_error.get_or_insert_with(|| {
                                CompilerError::Bail(BailOut {
                                    reason: "Failed to AST-render object method".to_string(),
                                    diagnostics: vec![CompilerDiagnostic {
                                        severity: DiagnosticSeverity::Invariant,
                                        message: format!(
                                            "object method AST render failed for key {key}"
                                        ),
                                    }],
                                })
                            });
                            format!("{key}: () => {{}}")
                        }
                    } else {
                        let val = codegen_place_to_expression(cx, &p.place);
                        if let Some(rendered) = function_expr_to_method_property(
                            &p.key,
                            computed_key_source.as_deref(),
                            &val,
                        ) {
                            rendered
                        } else {
                            format!("{}: {}", key, val)
                        }
                    }
                }
            }
        }
        ObjectPropertyOrSpread::Spread(p) => {
            format!("...{}", codegen_place_to_expression(cx, p))
        }
    }
}

fn codegen_object_property_key(cx: &mut Context, key: &ObjectPropertyKey) -> String {
    match key {
        ObjectPropertyKey::String(s) => {
            // If the string is a valid JS identifier, emit it unquoted
            if is_valid_js_identifier(s) {
                s.clone()
            } else {
                format!("\"{}\"", escape_string(s))
            }
        }
        ObjectPropertyKey::Identifier(s) => s.clone(),
        ObjectPropertyKey::Computed(place) => codegen_place_to_expression(cx, place),
        ObjectPropertyKey::Number(n) => format!("{}", n),
    }
}

fn codegen_object_property_key_str(key: &ObjectPropertyKey) -> String {
    match key {
        ObjectPropertyKey::String(s) => {
            // If the string is a valid JS identifier, emit it unquoted
            // This is important for destructuring patterns: { b } not { "b": b }
            if is_valid_js_identifier(s) {
                s.clone()
            } else {
                format!("\"{}\"", escape_string(s))
            }
        }
        ObjectPropertyKey::Identifier(s) => s.clone(),
        ObjectPropertyKey::Computed(_) => "[computed]".to_string(),
        ObjectPropertyKey::Number(n) => format!("{}", n),
    }
}

fn is_valid_js_identifier(s: &str) -> bool {
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

fn is_non_negative_integer_string(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|ch| ch.is_ascii_digit())
}

// ---- Method property resolution ----

/// Resolve a MethodCall property Place to either a dot-notation name or bracket-notation expression.
/// Returns (property_str, is_computed). For static props like `"push"` → ("push", false).
/// For computed props like `y.method` → ("y.method", true).
///
/// The `receiver_expr` parameter is the already-rendered receiver expression string. This allows
/// detection of cases where the property Place resolved to a member expression of the receiver
/// (e.g., `Symbol.for` when receiver is `Symbol`), extracting just the property name ("for")
/// for dot notation instead of incorrectly emitting bracket notation (`Symbol[Symbol.for](...)`).
fn resolve_method_property(cx: &mut Context, place: &Place, receiver_expr: &str) -> (String, bool) {
    let resolved = codegen_place_to_expression(cx, place);
    // Strip surrounding quotes if present (from Primitive::String → dot notation)
    if resolved.starts_with('"') && resolved.ends_with('"') && resolved.len() >= 2 {
        return (resolved[1..resolved.len() - 1].to_string(), false);
    }
    if let Some(ast_resolved) = resolve_method_property_via_ast(receiver_expr, &resolved) {
        return ast_resolved;
    }
    // Check if the resolved expression is a member expression of the receiver (e.g., `Symbol.for`
    // when receiver is `Symbol`). In this case, extract just the property name for dot notation.
    // This handles the pattern from PropagateEarlyReturns where MethodCall's property is a
    // PropertyLoad place that resolves to `<receiver>.<prop>`.
    if let Some(prop_name) = resolved.strip_prefix(receiver_expr) {
        if let Some(name) = prop_name.strip_prefix('.')
            && !name.is_empty()
            && is_valid_js_identifier_name(name)
        {
            return (name.to_string(), false);
        }
        if let Some(name) = prop_name.strip_prefix("?.")
            && !name.is_empty()
            && is_valid_js_identifier_name(name)
        {
            return (name.to_string(), false);
        }
        if let Some(expr) = extract_single_computed_member_suffix(prop_name) {
            return (expr, true);
        }
    }
    // Non-string property → computed/bracket notation
    (resolved, true)
}

fn normalize_rendered_expression_ast(expr: &str) -> Option<String> {
    let allocator = Allocator::default();
    let expression = parse_rendered_expression_ast(&allocator, expr)?;
    Some(codegen_expression_with_oxc(&expression))
}

fn resolve_method_property_via_ast(
    receiver_expr: &str,
    resolved_expr: &str,
) -> Option<(String, bool)> {
    let allocator = Allocator::default();
    let normalized_receiver = normalize_rendered_expression_ast(receiver_expr)?;
    let resolved = parse_rendered_expression_ast(&allocator, resolved_expr)?;

    fn member_property_from_expression(
        normalized_receiver: &str,
        expression: ast::Expression<'_>,
    ) -> Option<(String, bool)> {
        match expression {
            ast::Expression::StaticMemberExpression(member) => {
                if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                    Some((member.property.name.to_string(), false))
                } else {
                    None
                }
            }
            ast::Expression::ComputedMemberExpression(member) => {
                if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                    Some((codegen_expression_with_oxc(&member.expression), true))
                } else {
                    None
                }
            }
            ast::Expression::PrivateFieldExpression(member) => {
                if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                    Some((format!("#{}", member.field.name), false))
                } else {
                    None
                }
            }
            ast::Expression::ChainExpression(chain) => match chain.unbox().expression {
                ast::ChainElement::StaticMemberExpression(member) => {
                    if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                        Some((member.property.name.to_string(), false))
                    } else {
                        None
                    }
                }
                ast::ChainElement::ComputedMemberExpression(member) => {
                    if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                        Some((codegen_expression_with_oxc(&member.expression), true))
                    } else {
                        None
                    }
                }
                ast::ChainElement::PrivateFieldExpression(member) => {
                    if codegen_expression_with_oxc(&member.object) == normalized_receiver {
                        Some((format!("#{}", member.field.name), false))
                    } else {
                        None
                    }
                }
                _ => None,
            },
            ast::Expression::ParenthesizedExpression(parenthesized) => {
                member_property_from_expression(normalized_receiver, parenthesized.unbox().expression)
            }
            _ => None,
        }
    }

    member_property_from_expression(&normalized_receiver, resolved)
}

fn extract_single_computed_member_suffix(suffix: &str) -> Option<String> {
    let inner_start = if suffix.starts_with("?.[") {
        3
    } else if suffix.starts_with('[') {
        1
    } else {
        return None;
    };
    let bytes = suffix.as_bytes();
    let mut depth = 1i32;
    let mut index = inner_start;
    while index < bytes.len() {
        match bytes[index] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    if index + 1 == bytes.len() {
                        return Some(suffix[inner_start..index].to_string());
                    }
                    return None;
                }
            }
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = index.saturating_add(2);
                        continue;
                    }
                    if bytes[index] == quote {
                        break;
                    }
                    index += 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

// ---- Argument codegen ----

fn codegen_argument(cx: &mut Context, arg: &Argument) -> String {
    match arg {
        Argument::Place(p) => codegen_place_with_min_prec(cx, p, ExprPrecedence::Conditional),
        Argument::Spread(p) => format!("...{}", codegen_place_to_expression(cx, p)),
    }
}

fn join_call_arguments(rendered_args: &[String]) -> String {
    if rendered_args.len() >= 3
        && rendered_args
            .first()
            .is_some_and(|first| first.contains("=>"))
    {
        let mut lines: Vec<String> = Vec::with_capacity(rendered_args.len());
        lines.push(format!("{},", rendered_args[0]));
        for (index, arg) in rendered_args.iter().enumerate().skip(1) {
            if index + 1 < rendered_args.len() {
                lines.push(format!("{},", arg));
            } else {
                lines.push(arg.clone());
            }
        }
        lines.join("\n")
    } else {
        rendered_args.join(", ")
    }
}

fn should_break_optional_call_args(rendered_args: &[String]) -> bool {
    rendered_args.len() == 1 && rendered_args[0].trim_start().starts_with('<')
}

// ---- JSX codegen ----
const SINGLE_CHILD_FBT_TAGS: &[&str] = &["fbt:param", "fbs:param"];

fn codegen_jsx(
    cx: &mut Context,
    tag: &JsxTag,
    props: &[JsxAttribute],
    children: &Option<Vec<Place>>,
) -> String {
    let tag_name = match tag {
        JsxTag::BuiltinTag(name) => name.clone(),
        JsxTag::Component(place) => codegen_place_to_expression(cx, place),
        JsxTag::Fragment => return codegen_jsx_fragment(cx, children.as_deref().unwrap_or(&[])),
    };

    let attrs: Vec<String> = props
        .iter()
        .map(|attr| codegen_jsx_attribute(cx, attr))
        .collect();
    let attrs_str = if attrs.is_empty() {
        String::new()
    } else {
        format!(" {}", attrs.join(" "))
    };
    let single_child_fbt_tag = SINGLE_CHILD_FBT_TAGS.contains(&tag_name.as_str());

    if let Some(children) = children {
        if children.is_empty() {
            format!("<{}{} />", tag_name, attrs_str)
        } else {
            let child_strs: Vec<String> = if single_child_fbt_tag {
                children
                    .iter()
                    .map(|c| codegen_jsx_fbt_child(cx, c))
                    .collect()
            } else {
                children.iter().map(|c| codegen_jsx_child(cx, c)).collect()
            };
            format!(
                "<{}{}>{}</{}>",
                tag_name,
                attrs_str,
                child_strs.join(""),
                tag_name
            )
        }
    } else {
        format!("<{}{} />", tag_name, attrs_str)
    }
}

fn codegen_jsx_attribute(cx: &mut Context, attr: &JsxAttribute) -> String {
    match attr {
        JsxAttribute::Attribute { name, place } => {
            let mut val = codegen_place_to_expression(cx, place);
            val = val.trim().to_string();
            if val.starts_with('"') && val.ends_with('"') {
                let inner = &val[1..val.len() - 1];
                let is_fbt_operand = cx.fbt_operands.contains(&place.identifier.id);
                if jsx_attr_needs_expression_container(inner) && !is_fbt_operand {
                    let unicode_escaped = unicode_escape_non_ascii(inner);
                    format!("{}={{\"{}\"}}", name, unicode_escaped)
                } else {
                    if is_fbt_operand {
                        let raw_inner = &val[1..val.len() - 1];
                        if raw_inner.contains("\\\"") {
                            // Keep JSX attr form for fbt:param names, but rewrite escaped
                            // quotes to HTML entities so Babel/fbt can parse and transform.
                            let html_escaped = raw_inner.replace("\\\"", "&quot;");
                            let unicode_escaped = unicode_escape_non_ascii(&html_escaped);
                            format!("{}=\"{}\"", name, unicode_escaped)
                        } else {
                            let fbt_val = val
                                .replace("\\n", "\n")
                                .replace("\\r", "\r")
                                .replace("\\t", "\t");
                            format!("{}={}", name, fbt_val)
                        }
                    } else {
                        format!("{}={}", name, val)
                    }
                }
            } else {
                format!("{}={{{}}}", name, val)
            }
        }
        JsxAttribute::SpreadAttribute { argument } => {
            format!("{{...{}}}", codegen_place_to_expression(cx, argument))
        }
    }
}

fn codegen_jsx_child(cx: &mut Context, place: &Place) -> String {
    let ev = codegen_place_expr_value(cx, place);
    if ev.expr.starts_with('<') {
        ev.expr
    } else if ev.expr.starts_with('"') && ev.expr.ends_with('"') {
        let inner = &ev.expr[1..ev.expr.len() - 1];
        if ev.kind == ExprKind::JsxText {
            // JSXText: raw text unless contains <>&{}
            if jsx_text_needs_expression_container(inner) {
                let unicode_escaped = unicode_escape_non_ascii(inner);
                format!("{{\"{}\"}}", unicode_escaped)
            } else {
                inner.to_string()
            }
        } else {
            // Non-JSXText string -> expression container
            let unicode_escaped = unicode_escape_non_ascii(inner);
            format!("{{\"{}\"}}", unicode_escaped)
        }
    } else {
        format!("{{{}}}", ev.expr.trim())
    }
}

fn codegen_jsx_fbt_child(cx: &mut Context, place: &Place) -> String {
    let ev = codegen_place_expr_value(cx, place);
    if ev.kind == ExprKind::JsxText && ev.expr.starts_with('"') && ev.expr.ends_with('"') {
        return ev.expr[1..ev.expr.len() - 1].to_string();
    }
    if ev.expr.starts_with('<') {
        // Upstream treats JSX fragments in fbt:param as expression containers.
        if ev.expr.starts_with("<>") {
            return format!("{{{}}}", ev.expr);
        }
        return ev.expr;
    }
    format!("{{{}}}", ev.expr.trim())
}

fn codegen_jsx_fragment(cx: &mut Context, children: &[Place]) -> String {
    let child_strs: Vec<String> = children.iter().map(|c| codegen_jsx_child(cx, c)).collect();
    format!("<>{}</>", child_strs.join(""))
}

fn collect_inherited_decl_name_overrides_for_lowered_function(
    cx: &Context,
    lowered_func: &LoweredFunction,
) -> HashMap<DeclarationId, String> {
    fn maybe_add(cx: &Context, out: &mut HashMap<DeclarationId, String>, place: &Place) {
        let decl_id = place.identifier.declaration_id;
        if let Some(name) = cx.param_display_names.get(&decl_id) {
            out.entry(decl_id).or_insert_with(|| name.clone());
            return;
        }
        if let Some(name) = cx.declaration_name_overrides.get(&decl_id) {
            out.entry(decl_id).or_insert_with(|| name.clone());
        }
    }

    let mut inherited = HashMap::new();
    for param in &lowered_func.func.params {
        let place = match param {
            Argument::Place(p) | Argument::Spread(p) => p,
        };
        maybe_add(cx, &mut inherited, place);
    }
    for place in &lowered_func.func.context {
        maybe_add(cx, &mut inherited, place);
    }
    for (_, block) in &lowered_func.func.body.blocks {
        for instr in &block.instructions {
            crate::hir::visitors::for_each_instruction_lvalue(instr, |place| {
                maybe_add(cx, &mut inherited, place);
            });
            crate::hir::visitors::for_each_instruction_operand(instr, |place| {
                maybe_add(cx, &mut inherited, place);
            });
        }
        crate::hir::visitors::for_each_terminal_operand(&block.terminal, |place| {
            maybe_add(cx, &mut inherited, place);
        });
    }
    inherited
}

// ---- Function expression codegen ----

fn make_reactive_formal_params<'a>(
    builder: AstBuilder<'a>,
    params: &[Argument],
    param_names: &[String],
) -> Option<oxc_allocator::Box<'a, ast::FormalParameters<'a>>> {
    if params.len() != param_names.len() {
        return None;
    }
    let mut items = builder.vec();
    let mut rest = None;
    for (param, name) in params.iter().zip(param_names) {
        let pattern = builder.binding_pattern_binding_identifier(SPAN, builder.ident(name));
        match param {
            Argument::Place(_) => {
                items.push(builder.plain_formal_parameter(SPAN, pattern));
            }
            Argument::Spread(_) => {
                rest = Some(builder.alloc_formal_parameter_rest(
                    SPAN,
                    builder.vec(),
                    builder.binding_rest_element(SPAN, pattern),
                    NONE,
                ));
            }
        }
    }
    Some(builder.alloc(builder.formal_parameters(
        SPAN,
        ast::FormalParameterKind::FormalParameter,
        items,
        rest,
    )))
}

fn parse_function_body_for_ast_codegen<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    is_async: bool,
    is_generator: bool,
    body_source: &str,
) -> Result<ast::FunctionBody<'a>, String> {
    let async_prefix = if is_async { "async " } else { "" };
    let generator_prefix = if is_generator { "*" } else { "" };
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(body_source);
    let mut attempts = vec![
        (source_type, body_source.to_string()),
        (
            source_type.with_typescript(true),
            flow_cast_rewritten.clone(),
        ),
    ];
    if flow_cast_rewritten != body_source {
        attempts.push((source_type, flow_cast_rewritten));
    }

    for (attempt_source_type, attempt_body) in attempts {
        let wrapper = format!(
            "{}function {}__codex_codegen_body() {{\n{}\n}}",
            async_prefix, generator_prefix, attempt_body
        );
        let wrapper = allocator.alloc_str(&wrapper);
        let parsed = Parser::new(allocator, wrapper, attempt_source_type).parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            continue;
        }
        let Some(ast::Statement::FunctionDeclaration(function)) =
            parsed.program.body.into_iter().next()
        else {
            continue;
        };
        if let Some(body) = function.unbox().body {
            return Ok(body.unbox());
        }
    }

    Err("failed to parse nested function body".to_string())
}

fn parse_expression_for_ast_codegen<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    expr_source: &str,
) -> Result<ast::Expression<'a>, String> {
    let flow_cast_rewritten = crate::pipeline::rewrite_flow_cast_expressions(expr_source);
    let mut attempts = vec![
        (source_type, expr_source.to_string()),
        (
            source_type.with_typescript(true),
            flow_cast_rewritten.clone(),
        ),
    ];
    if flow_cast_rewritten != expr_source {
        attempts.push((source_type, flow_cast_rewritten));
    }

    for (attempt_source_type, attempt_expr) in attempts {
        let wrapper = format!("const __codex_expr = {attempt_expr};");
        let wrapper = allocator.alloc_str(&wrapper);
        let parsed = Parser::new(allocator, wrapper, attempt_source_type).parse();
        if parsed.panicked || !parsed.errors.is_empty() {
            continue;
        }
        let Some(ast::Statement::VariableDeclaration(declaration)) =
            parsed.program.body.into_iter().next()
        else {
            continue;
        };
        let Some(init) = declaration
            .unbox()
            .declarations
            .into_iter()
            .next()
            .and_then(|declarator| declarator.init)
        else {
            continue;
        };
        return Ok(init);
    }

    Err("failed to parse nested expression".to_string())
}

fn parse_statement_list_for_ast_codegen<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    statement_source: &str,
) -> Result<oxc_allocator::Vec<'a, ast::Statement<'a>>, String> {
    let parsed_body = parse_function_body_for_ast_codegen(
        allocator,
        source_type,
        false,
        false,
        statement_source,
    )?;
    if !parsed_body.directives.is_empty() {
        return Err("block statements contain directives".to_string());
    }
    Ok(parsed_body.statements)
}

fn parse_single_statement_for_ast_codegen<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    statement_source: &str,
) -> Result<ast::Statement<'a>, String> {
    let statements =
        parse_statement_list_for_ast_codegen(allocator, source_type, statement_source)?;
    if statements.len() != 1 {
        return Err("expected exactly one statement".to_string());
    }
    statements
        .into_iter()
        .next()
        .ok_or_else(|| "missing statement".to_string())
}

fn parse_block_statement_for_ast_codegen<'a>(
    allocator: &'a Allocator,
    source_type: SourceType,
    body_source: &str,
) -> Result<oxc_allocator::Box<'a, ast::BlockStatement<'a>>, String> {
    let builder = AstBuilder::new(allocator);
    let statements = parse_statement_list_for_ast_codegen(allocator, source_type, body_source)?;
    Ok(builder.alloc_block_statement(SPAN, statements))
}

fn single_return_expression_from_body<'a>(
    allocator: &'a Allocator,
    body: &ast::FunctionBody<'a>,
) -> Option<ast::Expression<'a>> {
    if !body.directives.is_empty() || body.statements.len() != 1 {
        return None;
    }
    let ast::Statement::ReturnStatement(statement) = &body.statements[0] else {
        return None;
    };
    statement
        .argument
        .as_ref()
        .map(|expression| expression.clone_in(allocator))
}

fn wrap_named_anonymous_function_expression<'a>(
    builder: AstBuilder<'a>,
    expression: ast::Expression<'a>,
    name_hint: &str,
) -> ast::Expression<'a> {
    let key_literal = builder.expression_string_literal(SPAN, builder.atom(name_hint), None);
    let property = builder.object_property_kind_object_property(
        SPAN,
        ast::PropertyKind::Init,
        ast::PropertyKey::from(builder.expression_string_literal(
            SPAN,
            builder.atom(name_hint),
            None,
        )),
        expression,
        false,
        false,
        false,
    );
    ast::Expression::from(builder.member_expression_computed(
        SPAN,
        builder.expression_object(SPAN, builder.vec1(property)),
        key_literal,
        false,
    ))
}

fn make_object_property_key_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    key: &ObjectPropertyKey,
    computed_key_source: Option<&str>,
) -> Option<(ast::PropertyKey<'a>, bool)> {
    match key {
        ObjectPropertyKey::Identifier(name) => Some((
            builder.property_key_static_identifier(SPAN, builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) if is_valid_js_identifier(name) => Some((
            builder.property_key_static_identifier(SPAN, builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) => Some((
            ast::PropertyKey::from(builder.expression_string_literal(
                SPAN,
                builder.atom(name),
                None,
            )),
            false,
        )),
        ObjectPropertyKey::Number(value) => Some((
            ast::PropertyKey::from(builder.expression_numeric_literal(
                SPAN,
                *value,
                None,
                oxc_syntax::number::NumberBase::Decimal,
            )),
            false,
        )),
        ObjectPropertyKey::Computed(_) => {
            let key_source = computed_key_source?;
            let key_expression = parse_expression_for_ast_codegen(
                allocator,
                SourceType::mjs().with_jsx(true),
                key_source,
            )
            .ok()?;
            Some((ast::PropertyKey::from(key_expression), true))
        }
    }
}

fn extract_single_object_property_source(rendered_object_expression: &str) -> Option<String> {
    let stripped = rendered_object_expression.trim();
    let stripped = stripped
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(stripped)
        .trim();
    let stripped = stripped
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))?;
    Some(stripped.trim().to_string())
}

fn codegen_object_property_with_oxc(property: ast::ObjectPropertyKind<'_>) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let expression = ast::Expression::from(
        builder.expression_object(SPAN, builder.vec1(property.clone_in(&allocator))),
    );
    let rendered = codegen_expression_with_oxc(&expression);
    extract_single_object_property_source(&rendered)
}

fn build_object_method_property_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    key: &ObjectPropertyKey,
    computed_key_source: Option<&str>,
    params: &[Argument],
    param_names: &[String],
    body_source: &str,
    is_async: bool,
    is_generator: bool,
) -> Option<ast::ObjectPropertyKind<'a>> {
    let parsed_body = parse_function_body_for_ast_codegen(
        allocator,
        SourceType::mjs().with_jsx(true),
        is_async,
        is_generator,
        body_source,
    )
    .ok()?;
    let formal_params = make_reactive_formal_params(builder, params, param_names)?;
    let function = builder.expression_function(
        SPAN,
        ast::FunctionType::FunctionExpression,
        None,
        is_generator,
        is_async,
        false,
        NONE,
        NONE,
        formal_params,
        NONE,
        Some(builder.alloc(parsed_body)),
    );
    let (property_key, computed) =
        make_object_property_key_ast(builder, allocator, key, computed_key_source)?;
    Some(builder.object_property_kind_object_property(
        SPAN,
        ast::PropertyKind::Init,
        property_key,
        function,
        true,
        false,
        computed,
    ))
}

fn render_object_method_ast(
    key: &ObjectPropertyKey,
    computed_key_source: Option<&str>,
    params: &[Argument],
    param_names: &[String],
    body_source: &str,
    directives: &[String],
    is_async: bool,
    is_generator: bool,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let property = build_object_method_property_ast(
        builder,
        &allocator,
        key,
        computed_key_source,
        params,
        param_names,
        body_source,
        is_async,
        is_generator,
    )?;
    let rendered = codegen_object_property_with_oxc(property)?;
    if directives.is_empty() {
        Some(rendered)
    } else {
        Some(rendered)
    }
}

fn build_function_property_from_value_ast<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    key: &ObjectPropertyKey,
    computed_key_source: Option<&str>,
    value_source: &str,
) -> Option<ast::ObjectPropertyKind<'a>> {
    let mut expression = parse_expression_for_ast_codegen(
        allocator,
        SourceType::mjs().with_jsx(true),
        value_source,
    )
    .ok()?;
    let function = loop {
        match expression {
            ast::Expression::FunctionExpression(function) => break function,
            ast::Expression::ParenthesizedExpression(parenthesized) => {
                expression = parenthesized.unbox().expression;
            }
            _ => return None,
        }
    };
    let function = ast::Expression::FunctionExpression(builder.alloc(function.unbox()));
    let (property_key, computed) =
        make_object_property_key_ast(builder, allocator, key, computed_key_source)?;
    Some(builder.object_property_kind_object_property(
        SPAN,
        ast::PropertyKind::Init,
        property_key,
        function,
        true,
        false,
        computed,
    ))
}

fn function_expr_to_method_property(
    key: &ObjectPropertyKey,
    computed_key_source: Option<&str>,
    value_source: &str,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let property = build_function_property_from_value_ast(
        builder,
        &allocator,
        key,
        computed_key_source,
        value_source,
    )?;
    codegen_object_property_with_oxc(property)
}

fn is_identifier_expression_named(expression: &ast::Expression<'_>, name: &str) -> bool {
    matches!(expression, ast::Expression::Identifier(identifier) if identifier.name == name)
}

fn render_object_expression_ast(
    cx: &mut Context,
    properties: &[ObjectPropertyOrSpread],
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut lowered = builder.vec();
    for property in properties {
        let property = match property {
            ObjectPropertyOrSpread::Spread(place) => builder.object_property_kind_spread_property(
                SPAN,
                parse_rendered_expression_ast(&allocator, &codegen_place_to_expression(cx, place))?,
            ),
            ObjectPropertyOrSpread::Property(property) => match property.type_ {
                ObjectPropertyType::Property => {
                    let value = parse_rendered_expression_ast(
                        &allocator,
                        &codegen_place_to_expression(cx, &property.place),
                    )?;
                    let computed_key_source = match &property.key {
                        ObjectPropertyKey::Computed(place) => {
                            Some(codegen_place_to_expression(cx, place))
                        }
                        _ => None,
                    };
                    let (key, computed) = make_object_property_key_ast(
                        builder,
                        &allocator,
                        &property.key,
                        computed_key_source.as_deref(),
                    )?;
                    let shorthand = match &property.key {
                        ObjectPropertyKey::Identifier(name) => {
                            is_identifier_expression_named(&value, name)
                        }
                        ObjectPropertyKey::String(name) if is_valid_js_identifier(name) => {
                            is_identifier_expression_named(&value, name)
                        }
                        _ => false,
                    };
                    builder.object_property_kind_object_property(
                        SPAN,
                        ast::PropertyKind::Init,
                        key,
                        value,
                        false,
                        shorthand,
                        computed,
                    )
                }
                ObjectPropertyType::Method => {
                    let computed_key_source = match &property.key {
                        ObjectPropertyKey::Computed(place) => {
                            Some(codegen_place_to_expression(cx, place))
                        }
                        _ => None,
                    };
                    if let Some(&idx) = cx.object_methods.get(&property.place.identifier.id) {
                        let lf = cx.object_methods_store[idx].lowered_func.clone();
                        let inner_hir = lf.func.clone();
                        let mut reactive_func =
                            super::build_reactive_function::build_reactive_function(inner_hir);
                        super::prune_unused_labels_reactive::prune_unused_labels(
                            &mut reactive_func,
                        );
                        super::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_func);
                        let _ = super::prune_hoisted_contexts::prune_hoisted_contexts(
                            &mut reactive_func,
                        );
                        let mut inner_result =
                            codegen_reactive_function_with_options_and_fbt_operands(
                                &reactive_func,
                                cx.unique_identifiers.clone(),
                                CodegenReactiveOptions {
                                    enable_name_anonymous_functions: cx
                                        .enable_name_anonymous_functions,
                                    ..CodegenReactiveOptions::default()
                                },
                                cx.fbt_operands.clone(),
                            );
                        adopt_codegen_error(cx, inner_result.error.take());
                        build_object_method_property_ast(
                            builder,
                            &allocator,
                            &property.key,
                            computed_key_source.as_deref(),
                            &lf.func.params,
                            &inner_result.param_names,
                            inner_result.body.trim(),
                            lf.func.async_,
                            lf.func.generator,
                        )?
                    } else {
                        let value_source = codegen_place_to_expression(cx, &property.place);
                        if let Some(method_property) = build_function_property_from_value_ast(
                            builder,
                            &allocator,
                            &property.key,
                            computed_key_source.as_deref(),
                            &value_source,
                        ) {
                            method_property
                        } else {
                            let value =
                                parse_rendered_expression_ast(&allocator, &value_source)?;
                            let (key, computed) = make_object_property_key_ast(
                                builder,
                                &allocator,
                                &property.key,
                                computed_key_source.as_deref(),
                            )?;
                            builder.object_property_kind_object_property(
                                SPAN,
                                ast::PropertyKind::Init,
                                key,
                                value,
                                false,
                                false,
                                computed,
                            )
                        }
                    }
                }
            },
        };
        lowered.push(property);
    }
    let expression = builder.expression_object(SPAN, lowered);
    Some(codegen_expression_with_oxc(&expression))
}

fn codegen_expression_with_oxc(expression: &ast::Expression<'_>) -> String {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let program = builder.program(
        SPAN,
        SourceType::mjs().with_jsx(true),
        "",
        builder.vec(),
        None,
        builder.vec(),
        builder.vec1(builder.statement_expression(SPAN, expression.clone_in(&allocator))),
    );
    let code = Codegen::new()
        .with_options(CodegenOptions {
            indent_char: IndentChar::Space,
            indent_width: 2,
            ..CodegenOptions::default()
        })
        .build(&program)
        .code;
    let trimmed = code.trim_end();
    trimmed.strip_suffix(';').unwrap_or(trimmed).to_string()
}

fn codegen_statement_with_oxc(statement: &ast::Statement<'_>) -> String {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let program = builder.program(
        SPAN,
        SourceType::mjs().with_jsx(true),
        "",
        builder.vec(),
        None,
        builder.vec(),
        builder.vec1(statement.clone_in(&allocator)),
    );
    Codegen::new()
        .with_options(CodegenOptions {
            indent_char: IndentChar::Space,
            indent_width: 2,
            ..CodegenOptions::default()
        })
        .build(&program)
        .code
        .trim_end()
        .to_string()
}

fn codegen_statements_with_oxc(statements: &[ast::Statement<'_>]) -> String {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let program = builder.program(
        SPAN,
        SourceType::mjs().with_jsx(true),
        "",
        builder.vec(),
        None,
        builder.vec(),
        builder.vec_from_iter(
            statements
                .iter()
                .map(|statement| statement.clone_in(&allocator)),
        ),
    );
    Codegen::new()
        .with_options(CodegenOptions {
            indent_char: IndentChar::Space,
            indent_width: 2,
            ..CodegenOptions::default()
        })
        .build(&program)
        .code
        .trim_end()
        .to_string()
}

fn codegen_directives_and_statements_with_oxc(
    directives: &[String],
    statements: &[ast::Statement<'_>],
) -> String {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let program = builder.program(
        SPAN,
        SourceType::mjs().with_jsx(true),
        "",
        builder.vec(),
        None,
        builder.vec_from_iter(directives.iter().map(|directive| {
            builder.directive(
                SPAN,
                builder.string_literal(SPAN, directive.as_str(), None),
                directive.as_str(),
            )
        })),
        builder.vec_from_iter(
            statements
                .iter()
                .map(|statement| statement.clone_in(&allocator)),
        ),
    );
    Codegen::new()
        .with_options(CodegenOptions {
            indent_char: IndentChar::Space,
            indent_width: 2,
            ..CodegenOptions::default()
        })
        .build(&program)
        .code
        .trim_end()
        .to_string()
}

fn split_flow_cast_expression_source(expr: &str) -> Option<(&str, &str)> {
    let trimmed = expr.trim();
    let inner = trimmed.strip_prefix('(')?.strip_suffix(')')?;
    let chars: Vec<(usize, char)> = inner.char_indices().collect();
    let mut depth_paren = 0usize;
    let mut depth_brace = 0usize;
    let mut depth_bracket = 0usize;
    let mut ternary_depth = 0usize;
    let mut colon_at: Option<usize> = None;

    for (idx, (byte_idx, ch)) in chars.iter().enumerate() {
        match ch {
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '{' => depth_brace += 1,
            '}' => depth_brace = depth_brace.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            _ => {}
        }

        let at_top = depth_paren == 0 && depth_brace == 0 && depth_bracket == 0;
        if !at_top {
            continue;
        }

        let prev = if idx > 0 { chars[idx - 1].1 } else { '\0' };
        let next = if idx + 1 < chars.len() {
            chars[idx + 1].1
        } else {
            '\0'
        };

        match ch {
            '?' if prev != '?' && next != '?' => {
                ternary_depth += 1;
            }
            ':' => {
                if ternary_depth > 0 {
                    ternary_depth -= 1;
                } else {
                    colon_at = Some(*byte_idx);
                }
            }
            _ => {}
        }
    }

    let colon = colon_at?;
    let left = inner[..colon].trim();
    let right = inner[colon + 1..].trim();
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((left, right))
}

fn is_quoted_string_literal_source(expr: &str) -> bool {
    let trimmed = expr.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    matches!(first, '"' | '\'') && trimmed.ends_with(first) && trimmed.len() >= 2
}

fn contains_flow_cast_expression_source(expr: &str) -> bool {
    crate::pipeline::rewrite_flow_cast_expressions(expr) != expr
}

fn render_reactive_variable_statement_ast(
    kind: ast::VariableDeclarationKind,
    name: &str,
    init: Option<&str>,
) -> Option<String> {
    if let Some(expr) = init.filter(|expr| {
        split_flow_cast_expression_source(expr).is_some()
            || contains_flow_cast_expression_source(expr)
    }) {
        let keyword = match kind {
            ast::VariableDeclarationKind::Const => "const",
            _ => "let",
        };
        return Some(format!("{keyword} {name} = {expr};\n"));
    }
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let init_expression = match init.filter(|expr| *expr != "undefined") {
        Some(expr) => Some(
            parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), expr)
                .ok()?,
        ),
        None => None,
    };
    let statement = ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        SPAN,
        kind,
        builder.vec1(builder.variable_declarator(
            SPAN,
            kind,
            builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)),
            NONE,
            init_expression,
            false,
        )),
        false,
    ));
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_variable_declaration_header_ast(
    kind: ast::VariableDeclarationKind,
    declarators: &[(String, Option<String>)],
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut rendered = builder.vec();
    for (name, init) in declarators {
        let init_expression = match init {
            Some(expr) => Some(
                parse_expression_for_ast_codegen(
                    &allocator,
                    SourceType::mjs().with_jsx(true),
                    expr,
                )
                .ok()?,
            ),
            None => None,
        };
        rendered.push(builder.variable_declarator(
            SPAN,
            kind,
            builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)),
            NONE,
            init_expression,
            false,
        ));
    }
    let statement = ast::Statement::VariableDeclaration(builder.alloc_variable_declaration(
        SPAN, kind, rendered, false,
    ));
    Some(
        codegen_statement_with_oxc(&statement)
            .trim_end_matches(';')
            .to_string(),
    )
}

fn render_reactive_declare_local_statement_ast(name: &str) -> Option<String> {
    render_reactive_variable_statement_ast(ast::VariableDeclarationKind::Let, name, None)
}

fn build_identifier_assignment_statement_ast<'a>(
    builder: AstBuilder<'a>,
    target_name: &str,
    value_name: &str,
) -> ast::Statement<'a> {
    build_identifier_assignment_statement_ast_with_expression(
        builder,
        target_name,
        builder.expression_identifier(SPAN, builder.ident(value_name)),
    )
}

fn render_reactive_assignment_statement_ast(target_name: &str, rhs: &str) -> Option<String> {
    if split_flow_cast_expression_source(rhs).is_some()
        || contains_flow_cast_expression_source(rhs)
    {
        return Some(format!("{target_name} = {rhs};\n"));
    }
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let rhs_expression =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), rhs).ok()?;
    let statement = build_identifier_assignment_statement_ast_with_expression(
        builder,
        target_name,
        rhs_expression,
    );
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_expression_statement_ast(expression: &str) -> Option<String> {
    if split_flow_cast_expression_source(expression).is_some()
        || contains_flow_cast_expression_source(expression)
    {
        return Some(format!("{expression};\n"));
    }
    if is_quoted_string_literal_source(expression) {
        return Some(format!("{expression};\n"));
    }
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_expression =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), expression)
            .ok()?;
    let statement = builder.statement_expression(SPAN, parsed_expression);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_break_statement_ast(label: Option<&str>) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let statement = builder.statement_break(
        SPAN,
        label.map(|label| builder.label_identifier(SPAN, builder.atom(label))),
    );
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_continue_statement_ast(label: Option<&str>) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let statement = builder.statement_continue(
        SPAN,
        label.map(|label| builder.label_identifier(SPAN, builder.atom(label))),
    );
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_return_statement_ast(argument: Option<&str>) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_argument = match argument {
        Some(argument) => Some(
            parse_expression_for_ast_codegen(
                &allocator,
                SourceType::mjs().with_jsx(true),
                argument,
            )
            .ok()?,
        ),
        None => None,
    };
    let statement = builder.statement_return(SPAN, parsed_argument);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_throw_statement_ast(argument: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_argument =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), argument)
            .ok()?;
    let statement = builder.statement_throw(SPAN, parsed_argument);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_if_statement_ast(
    test: &str,
    consequent_body: &str,
    alternate_body: Option<&str>,
) -> Option<String> {
    if contains_flow_cast_expression_source(test)
        || contains_flow_cast_expression_source(consequent_body)
        || alternate_body.is_some_and(contains_flow_cast_expression_source)
    {
        return None;
    }
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_test =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), test)
            .ok()?;
    let consequent = builder.statement_block(
        SPAN,
        parse_statement_list_for_ast_codegen(
            &allocator,
            SourceType::mjs().with_jsx(true),
            consequent_body,
        )
        .ok()?,
    );
    let alternate = match alternate_body.filter(|body| !body.trim().is_empty()) {
        Some(body) => Some(
            builder.statement_block(
                SPAN,
                parse_statement_list_for_ast_codegen(
                    &allocator,
                    SourceType::mjs().with_jsx(true),
                    body,
                )
                .ok()?,
            ),
        ),
        None => None,
    };
    let statement = builder.statement_if(SPAN, parsed_test, consequent, alternate);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_labeled_statement_ast(
    label: &str,
    statement_source: &str,
    wrap_in_block: bool,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let body = if wrap_in_block {
        builder.statement_block(
            SPAN,
            parse_statement_list_for_ast_codegen(
                &allocator,
                SourceType::mjs().with_jsx(true),
                statement_source,
            )
            .ok()?,
        )
    } else {
        parse_single_statement_for_ast_codegen(
            &allocator,
            SourceType::mjs().with_jsx(true),
            statement_source,
        )
        .ok()?
    };
    let statement =
        builder.statement_labeled(SPAN, builder.label_identifier(SPAN, builder.atom(label)), body);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_while_statement_ast(test: &str, body: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_test =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), test)
            .ok()?;
    let parsed_body = builder.statement_block(
        SPAN,
        parse_statement_list_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), body)
            .ok()?,
    );
    let statement = builder.statement_while(SPAN, parsed_test, parsed_body);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_do_while_statement_ast(body: &str, test: &str) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_body = builder.statement_block(
        SPAN,
        parse_statement_list_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), body)
            .ok()?,
    );
    let parsed_test =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), test)
            .ok()?;
    let statement = builder.statement_do_while(SPAN, parsed_body, parsed_test);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_try_statement_ast(
    try_body: &str,
    catch_param: Option<&str>,
    catch_body: Option<&str>,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let try_block = parse_block_statement_for_ast_codegen(
        &allocator,
        SourceType::mjs().with_jsx(true),
        try_body,
    )
    .ok()?;
    let body = catch_body?;
    let param = catch_param.map(|name| {
        builder.catch_parameter(
            SPAN,
            builder.binding_pattern_binding_identifier(SPAN, builder.ident(name)),
            NONE,
        )
    });
    let handler = builder.alloc_catch_clause(
        SPAN,
        param,
        parse_block_statement_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), body)
            .ok()?,
    );
    let statement = builder.statement_try(
        SPAN,
        try_block,
        Some(handler),
        Option::<oxc_allocator::Box<'_, ast::BlockStatement<'_>>>::None,
    );
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_switch_statement_ast(
    test: &str,
    cases: &[(Option<String>, Option<String>)],
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let parsed_test =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), test)
            .ok()?;
    let mut rendered_cases = builder.vec();
    for (case_test, block_code) in cases {
        let parsed_case_test = match case_test {
            Some(case_test) => Some(
                parse_expression_for_ast_codegen(
                    &allocator,
                    SourceType::mjs().with_jsx(true),
                    case_test,
                )
                .ok()?,
            ),
            None => None,
        };
        let consequent = if let Some(block_code) = block_code {
            builder.vec1(
                builder.statement_block(
                    SPAN,
                    parse_statement_list_for_ast_codegen(
                        &allocator,
                        SourceType::mjs().with_jsx(true),
                        block_code,
                    )
                    .ok()?,
                ),
            )
        } else {
            builder.vec()
        };
        rendered_cases.push(builder.switch_case(SPAN, parsed_case_test, consequent));
    }
    let statement = builder.statement_switch(SPAN, parsed_test, rendered_cases);
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn render_reactive_for_statement_ast(
    init: &str,
    test: &str,
    update: &str,
    body: &str,
) -> Option<String> {
    let statement_source = format!("for ({init}; {test}; {update}) {{\n{body}}}");
    render_single_statement_source_with_oxc(&statement_source)
}

fn render_reactive_for_of_statement_ast(
    kind: &str,
    lvalue: &str,
    collection: &str,
    body: &str,
) -> Option<String> {
    let statement_source = format!("for ({kind} {lvalue} of {collection}) {{\n{body}}}");
    render_single_statement_source_with_oxc(&statement_source)
}

fn render_reactive_for_in_statement_ast(
    kind: &str,
    lvalue: &str,
    collection: &str,
    body: &str,
) -> Option<String> {
    let statement_source = format!("for ({kind} {lvalue} in {collection}) {{\n{body}}}");
    render_single_statement_source_with_oxc(&statement_source)
}

fn render_reactive_function_body_prologue_ast(
    directives: Option<&[String]>,
    cache_prologue: Option<&CachePrologue>,
) -> Option<String> {
    let allocator = Allocator::default();
    let builder = AstBuilder::new(&allocator);
    let mut statements: Vec<ast::Statement<'_>> = Vec::new();
    let directives = directives.unwrap_or(&[]);

    if let Some(cache_prologue) = cache_prologue {
        statements.push(ast::Statement::VariableDeclaration(
            builder.alloc_variable_declaration(
                SPAN,
                ast::VariableDeclarationKind::Const,
                builder.vec1(
                    builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Const,
                        builder.binding_pattern_binding_identifier(
                            SPAN,
                            builder.ident(&cache_prologue.binding_name),
                        ),
                        NONE,
                        Some(
                            parse_expression_for_ast_codegen(
                                &allocator,
                                SourceType::mjs().with_jsx(true),
                                &format!("_c({})", cache_prologue.size),
                            )
                            .ok()?,
                        ),
                        false,
                    ),
                ),
                false,
            ),
        ));

        if let Some(fast_refresh) = &cache_prologue.fast_refresh {
            let refresh_statement = parse_single_statement_for_ast_codegen(
                &allocator,
                SourceType::mjs().with_jsx(true),
                &format!(
                    "if (\n  {}[{}] !== \"{}\"\n) {{\n  for (let {} = 0; {} < {}; {} += 1) {{\n    {}[{}] = Symbol.for(\"{}\");\n  }}\n  {}[{}] = \"{}\";\n}}",
                    cache_prologue.binding_name,
                    fast_refresh.cache_index,
                    escape_string(&fast_refresh.hash),
                    fast_refresh.index_binding_name,
                    fast_refresh.index_binding_name,
                    cache_prologue.size,
                    fast_refresh.index_binding_name,
                    cache_prologue.binding_name,
                    fast_refresh.index_binding_name,
                    MEMO_CACHE_SENTINEL,
                    cache_prologue.binding_name,
                    fast_refresh.cache_index,
                    escape_string(&fast_refresh.hash),
                ),
            )
            .ok()?;
            statements.push(refresh_statement);
        }
    }

    if directives.is_empty() && statements.is_empty() {
        None
    } else {
        Some(format!(
            "{}\n",
            codegen_directives_and_statements_with_oxc(directives, &statements)
        ))
    }
}

fn render_cached_inline_hook_callback_block_ast(
    callback_name: &str,
    callback_expr: &str,
    callback_slot: u32,
    deps: Option<(&str, &str, u32)>,
    call_expr: &str,
) -> Option<String> {
    let mut output = String::new();
    output.push_str(&render_reactive_variable_statement_ast(
        ast::VariableDeclarationKind::Let,
        callback_name,
        None,
    )?);
    if let Some((deps_name, _, _)) = deps {
        output.push_str(&render_reactive_variable_statement_ast(
            ast::VariableDeclarationKind::Let,
            deps_name,
            None,
        )?);
    }

    let mut consequent = String::new();
    consequent.push_str(&render_reactive_assignment_statement_ast(
        callback_name,
        callback_expr,
    )?);
    if let Some((deps_name, deps_expr, deps_slot)) = deps {
        consequent.push_str(&render_reactive_assignment_statement_ast(
            deps_name, deps_expr,
        )?);
        consequent.push_str(&render_reactive_expression_statement_ast(&format!(
            "$[{}] = {}",
            deps_slot, deps_name
        ))?);
    }
    consequent.push_str(&render_reactive_expression_statement_ast(&format!(
        "$[{}] = {}",
        callback_slot, callback_name
    ))?);

    let mut alternate =
        render_reactive_assignment_statement_ast(callback_name, &format!("$[{}]", callback_slot))?;
    if let Some((deps_name, _, deps_slot)) = deps {
        alternate.push_str(&render_reactive_assignment_statement_ast(
            deps_name,
            &format!("$[{}]", deps_slot),
        )?);
    }

    output.push_str(&render_reactive_if_statement_ast(
        &format!(
            "$[{}] === Symbol.for(\"{}\")",
            callback_slot, MEMO_CACHE_SENTINEL
        ),
        &consequent,
        Some(&alternate),
    )?);
    output.push_str(&render_reactive_expression_statement_ast(call_expr)?);
    Some(output)
}

fn render_single_statement_source_with_oxc(statement_source: &str) -> Option<String> {
    let allocator = Allocator::default();
    let statement = parse_single_statement_for_ast_codegen(
        &allocator,
        SourceType::mjs().with_jsx(true),
        statement_source,
    )
    .ok()?;
    Some(format!("{}\n", codegen_statement_with_oxc(&statement)))
}

fn build_identifier_assignment_statement_ast_with_expression<'a>(
    builder: AstBuilder<'a>,
    target_name: &str,
    rhs_expression: ast::Expression<'a>,
) -> ast::Statement<'a> {
    builder.statement_expression(
        SPAN,
        builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(
                builder.simple_assignment_target_assignment_target_identifier(
                    SPAN,
                    builder.ident(target_name),
                ),
            ),
            rhs_expression,
        ),
    )
}

fn render_function_expression_ast(
    params: &[Argument],
    param_names: &[String],
    body_source: &str,
    directives: &[String],
    is_async: bool,
    is_generator: bool,
    name: Option<&str>,
    fn_type: &FunctionExpressionType,
    anonymous_name_hint: Option<&str>,
) -> Option<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::mjs().with_jsx(true);
    let parsed_body = parse_function_body_for_ast_codegen(
        &allocator,
        source_type,
        is_async,
        is_generator,
        body_source,
    )
    .ok()?;
    let builder = AstBuilder::new(&allocator);
    let formal_params = make_reactive_formal_params(builder, params, param_names)?;
    let expression = match fn_type {
        FunctionExpressionType::ArrowFunctionExpression => {
            if directives.is_empty()
                && let Some(return_expr) =
                    single_return_expression_from_body(&allocator, &parsed_body)
            {
                builder.expression_arrow_function(
                    SPAN,
                    true,
                    is_async,
                    NONE,
                    formal_params,
                    NONE,
                    builder.alloc(builder.function_body(
                        SPAN,
                        builder.vec(),
                        builder.vec1(builder.statement_expression(SPAN, return_expr)),
                    )),
                )
            } else {
                builder.expression_arrow_function(
                    SPAN,
                    false,
                    is_async,
                    NONE,
                    formal_params,
                    NONE,
                    builder.alloc(parsed_body),
                )
            }
        }
        FunctionExpressionType::FunctionExpression
        | FunctionExpressionType::FunctionDeclaration => builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            name.map(|name| builder.binding_identifier(SPAN, builder.atom(name))),
            is_generator,
            is_async,
            false,
            NONE,
            NONE,
            formal_params,
            NONE,
            Some(builder.alloc(parsed_body)),
        ),
    };
    let expression = if let Some(name_hint) = anonymous_name_hint {
        wrap_named_anonymous_function_expression(builder, expression, name_hint)
    } else {
        expression
    };
    Some(codegen_expression_with_oxc(&expression))
}

fn function_expr_as_declaration(name: &str, rhs: &str) -> Option<String> {
    let allocator = Allocator::default();
    let mut expression =
        parse_expression_for_ast_codegen(&allocator, SourceType::mjs().with_jsx(true), rhs).ok()?;
    let function = loop {
        match expression {
            ast::Expression::FunctionExpression(function) => break function,
            ast::Expression::ParenthesizedExpression(parenthesized) => {
                expression = parenthesized.unbox().expression;
            }
            _ => return None,
        }
    };
    let builder = AstBuilder::new(&allocator);
    let mut function = function.unbox();
    function.r#type = ast::FunctionType::FunctionDeclaration;
    if function.id.is_none() {
        function.id = Some(builder.binding_identifier(SPAN, builder.atom(name)));
    }
    Some(codegen_statement_with_oxc(
        &ast::Statement::FunctionDeclaration(builder.alloc(function)),
    ))
}

fn codegen_function_expression(
    cx: &mut Context,
    lowered_func: &LoweredFunction,
    name: &Option<String>,
    fn_type: &FunctionExpressionType,
) -> String {
    // Clone the inner HIR function and build a ReactiveFunction from it,
    // then run full codegen with reactive scopes (matching upstream behavior).
    let inner_hir = lowered_func.func.clone();
    let mut reactive_func = super::build_reactive_function::build_reactive_function(inner_hir);

    // Run prune passes (matching upstream CodegenReactiveFunction.ts lines 2316-2321)
    super::prune_unused_labels_reactive::prune_unused_labels(&mut reactive_func);
    super::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_func);
    let _ = super::prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_func);

    // Preserve parent-resolved names for outer declaration IDs referenced by
    // the inner lowered function body (not only explicit context captures).
    let inherited_decl_name_overrides =
        collect_inherited_decl_name_overrides_for_lowered_function(cx, lowered_func);
    if std::env::var("DEBUG_INNER_CAPTURE_MAP").is_ok() {
        let mut pairs: Vec<(u32, String)> = inherited_decl_name_overrides
            .iter()
            .map(|(decl, name)| (decl.0, name.clone()))
            .collect();
        pairs.sort_by_key(|(decl, _)| *decl);
        eprintln!("[INNER_CAPTURE_MAP] fn={:?} inherited={:?}", name, pairs);
    }

    // Run recursive codegen on the inner function
    let mut inner_result = codegen_reactive_function_with_primitives(
        &reactive_func,
        cx.unique_identifiers.clone(),
        CodegenReactiveInputs {
            inline_primitive_literals: cx.primitive_literals_for_child(),
            inherited_declaration_name_overrides: inherited_decl_name_overrides,
            initial_temp_snapshot: cx.snapshot_temps(),
            fbt_operands: cx.fbt_operands.clone(),
            inherited_reserved_child_decl_names: HashSet::new(),
            emit_function_hook_guard: false,
        },
        cx.child_codegen_options(),
    );
    adopt_codegen_error(cx, inner_result.error.take());

    let body_trimmed = inner_result.body.trim();
    if let Some(rendered) = render_function_expression_ast(
        &lowered_func.func.params,
        &inner_result.param_names,
        body_trimmed,
        &lowered_func.func.directives,
        lowered_func.func.async_,
        lowered_func.func.generator,
        name.as_deref(),
        fn_type,
        if cx.enable_name_anonymous_functions && name.is_none() {
            lowered_func.func.id.as_deref()
        } else {
            None
        },
    ) {
        rendered
    } else {
        cx.codegen_error.get_or_insert_with(|| {
            CompilerError::Bail(BailOut {
                reason: "Failed to AST-render nested function expression".to_string(),
                diagnostics: vec![CompilerDiagnostic {
                    severity: DiagnosticSeverity::Invariant,
                    message: lowered_func.func.id.as_ref().map_or_else(
                        || "nested function expression AST render failed".to_string(),
                        |id| format!("nested function expression AST render failed: {id}"),
                    ),
                }],
            })
        });
        "(() => {})".to_string()
    }
}

fn compact_single_statement(code: &str) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() && !prev_space {
            out.push(' ');
        }
        out.push_str(trimmed);
        prev_space = out.ends_with(' ');
    }
    out
}

// ---- For-loop init/update helpers ----

fn codegen_for_init(cx: &mut Context, init_block: &ReactiveBlock) -> String {
    // The init block typically contains variable declarations
    let preserve_snapshot = cx.preserve_loop_header_inits;
    cx.preserve_loop_header_inits = true;
    let code = codegen_block(cx, init_block);
    cx.preserve_loop_header_inits = preserve_snapshot;
    if let Some(reconstructed) = reconstruct_for_init_declaration(&code) {
        return reconstructed;
    }
    let trimmed = code.trim().trim_end_matches(';');
    trimmed.to_string()
}

fn codegen_for_update(cx: &mut Context, update_block: &ReactiveBlock) -> String {
    fn collapse_statement_lines_to_sequence_expr(raw: &str) -> String {
        let mut exprs: Vec<String> = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let expr = trimmed.trim_end_matches(';').trim();
            if expr.is_empty()
                || expr.starts_with("let ")
                || expr.starts_with("const ")
                || expr.starts_with("var ")
                || expr.starts_with("if ")
                || expr.starts_with("for ")
                || expr.starts_with("while ")
                || expr.starts_with("switch ")
                || expr.starts_with("return ")
                || expr.starts_with("throw ")
                || expr.starts_with("try ")
            {
                return raw.trim().trim_end_matches(';').to_string();
            }
            exprs.push(expr.to_string());
        }

        match exprs.len() {
            0 => raw.trim().trim_end_matches(';').to_string(),
            1 => exprs.pop().unwrap_or_default(),
            _ => exprs.join(", "),
        }
    }

    let preserve_snapshot = cx.preserve_loop_header_inits;
    let temp_snapshot = cx.snapshot_temps();
    let decl_snapshot = cx.declarations.clone();
    cx.block_scope_output_names.push(HashSet::new());
    cx.block_scope_declared_temp_names.push(HashSet::new());
    cx.preserve_loop_header_inits = true;
    let code = codegen_block_no_reset(cx, update_block);
    let mut trimmed = code.trim().trim_end_matches(';').to_string();

    if trimmed.is_empty()
        && let Some(last_lvalue_decl) = update_block.iter().rev().find_map(|stmt| match stmt {
            ReactiveStatement::Instruction(instr) => instr
                .lvalue
                .as_ref()
                .map(|lvalue| lvalue.identifier.declaration_id),
            _ => None,
        })
        && let Some(expr) = cx
            .temp
            .get(&last_lvalue_decl)
            .and_then(|expr| expr.as_ref())
    {
        trimmed = expr.expr.clone();
    }
    if trimmed.contains('\n') {
        trimmed = collapse_statement_lines_to_sequence_expr(&trimmed);
    }

    let _ = cx.block_scope_declared_temp_names.pop();
    let _ = cx.block_scope_output_names.pop();
    cx.preserve_loop_header_inits = preserve_snapshot;
    cx.restore_temps(temp_snapshot);
    cx.declarations = decl_snapshot;
    trimmed
}

fn maybe_fill_for_header_initializer_from_update(
    init_code: &str,
    update_code: &str,
) -> Option<String> {
    fn parse_single_declaration_header(init: &str) -> Option<(ast::VariableDeclarationKind, String)> {
        let init = init.trim();
        let (kind, name) = if let Some(name) = init.strip_prefix("let ") {
            (ast::VariableDeclarationKind::Let, name)
        } else if let Some(name) = init.strip_prefix("const ") {
            (ast::VariableDeclarationKind::Const, name)
        } else if let Some(name) = init.strip_prefix("var ") {
            (ast::VariableDeclarationKind::Var, name)
        } else {
            return None;
        };
        let name = name.trim();
        if name.is_empty() || !is_simple_identifier_name(name) {
            return None;
        }
        Some((kind, name.to_string()))
    }

    let init = init_code.trim();
    let update = update_code.trim();
    if init.is_empty() || update.is_empty() {
        return None;
    }
    if init.contains('=') || init.contains(',') {
        return None;
    }
    if !update.is_empty() && update.contains(';') {
        return None;
    }
    if !is_inlineable_primitive_literal_expression(update) {
        return None;
    }
    let is_decl =
        init.starts_with("let ") || init.starts_with("const ") || init.starts_with("var ");
    if !is_decl {
        return None;
    }
    let (kind, name) = parse_single_declaration_header(init)?;
    render_variable_declaration_header_ast(kind, &[(name, Some(update.to_string()))])
}

fn reconstruct_for_init_declaration(code: &str) -> Option<String> {
    #[derive(Clone)]
    struct Declarator {
        name: String,
        init: Option<String>,
    }

    fn parse_decl_line(line: &str) -> Option<(&str, &str)> {
        let line = line.trim().trim_end_matches(';').trim();
        let (kind, rest) = if let Some(rest) = line.strip_prefix("let ") {
            ("let", rest)
        } else if let Some(rest) = line.strip_prefix("const ") {
            ("const", rest)
        } else if let Some(rest) = line.strip_prefix("var ") {
            ("var", rest)
        } else {
            return None;
        };
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        Some((kind, rest))
    }

    fn parse_simple_assignment_line(line: &str) -> Option<(String, String)> {
        let line = line.trim().trim_end_matches(';').trim();
        let (lhs, rhs) = line.split_once('=')?;
        let lhs = lhs.trim();
        let rhs = rhs.trim();
        if lhs.is_empty() || rhs.is_empty() || !is_simple_identifier_name(lhs) {
            return None;
        }
        Some((lhs.to_string(), rhs.to_string()))
    }

    let mut declarators: Vec<Declarator> = Vec::new();
    let mut header_kind: Option<&str> = None;

    for line in code.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((kind, decl_src)) = parse_decl_line(trimmed) {
            let kind = if kind == "var" { "let" } else { kind };
            header_kind = Some(match (header_kind, kind) {
                (Some("let"), _) | (_, "let") => "let",
                (Some(existing), _) => existing,
                (None, current) => current,
            });
            let (name, init) = if let Some((name, init)) = decl_src.split_once('=') {
                (name.trim().to_string(), Some(init.trim().to_string()))
            } else {
                (decl_src.to_string(), None)
            };
            if name.is_empty() {
                return None;
            }
            declarators.push(Declarator { name, init });
            continue;
        }

        if let Some((lhs, rhs)) = parse_simple_assignment_line(trimmed)
            && let Some(last) = declarators.last_mut()
            && last.init.is_none()
            && last.name == lhs
        {
            last.init = Some(rhs);
            continue;
        }

        return None;
    }

    if declarators.is_empty() {
        return None;
    }

    let kind = match header_kind.unwrap_or("let") {
        "const" => ast::VariableDeclarationKind::Const,
        "var" => ast::VariableDeclarationKind::Var,
        _ => ast::VariableDeclarationKind::Let,
    };
    render_variable_declaration_header_ast(
        kind,
        &declarators
            .into_iter()
            .map(|decl| (decl.name, decl.init))
            .collect::<Vec<_>>(),
    )
}

fn loop_lvalue_kind_keyword(kind: InstructionKind) -> &'static str {
    match kind {
        InstructionKind::Const => "const",
        InstructionKind::Let => "let",
        _ => "const",
    }
}

/// Extract `(kind, lvalue, collection)` for for-in/of headers from the
/// terminal-owned init payload. This mirrors upstream where the iterator
/// collection and loop assignment are encoded as instruction values.
fn extract_for_in_of_header_from_init(
    cx: &mut Context,
    init_block: &[ReactiveStatement],
) -> (String, String, Option<Place>) {
    let mut decl_kind = "const".to_string();
    let mut decl_lvalue = "_".to_string();
    let mut collection_place: Option<Place> = None;

    for stmt in init_block {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };

        match &instr.value {
            InstructionValue::IteratorNext { collection, .. } => {
                collection_place = Some(collection.clone());
            }
            InstructionValue::NextPropertyOf { value, .. } => {
                collection_place = Some(value.clone());
            }
            InstructionValue::StoreLocal { lvalue, .. } => {
                decl_kind = loop_lvalue_kind_keyword(lvalue.kind).to_string();
                decl_lvalue = identifier_name_with_cx(cx, &lvalue.place.identifier);
                cx.declare(&lvalue.place.identifier);
            }
            InstructionValue::Destructure { lvalue, .. } => {
                decl_kind = loop_lvalue_kind_keyword(lvalue.kind).to_string();
                decl_lvalue = codegen_pattern(cx, &lvalue.pattern);
                for p in pattern_operands(&lvalue.pattern) {
                    cx.declare(&p.identifier);
                }
            }
            InstructionValue::DeclareLocal { .. } => {
                // Keep scanning: declaration often precedes the assignment
                // that carries the final lvalue kind/pattern.
            }
            _ => {}
        }
    }

    (decl_kind, decl_lvalue, collection_place)
}

// ---- Primitive codegen ----

fn codegen_primitive(value: &PrimitiveValue) -> String {
    match value {
        PrimitiveValue::Null => "null".to_string(),
        PrimitiveValue::Undefined => "undefined".to_string(),
        PrimitiveValue::Boolean(b) => b.to_string(),
        PrimitiveValue::Number(n) => {
            if *n == 0.0 && n.is_sign_negative() {
                // Negative zero: emit as 0
                "0".to_string()
            } else if *n < 0.0 {
                format!("-{}", -n)
            } else {
                format!("{}", n)
            }
        }
        PrimitiveValue::String(s) => format!("\"{}\"", escape_string(s)),
    }
}

// ---- Template literal codegen ----

fn codegen_template_literal(
    cx: &mut Context,
    quasis: &[TemplateQuasi],
    subexprs: &[Place],
) -> String {
    // Empty template literal with no interpolations -> ""
    if subexprs.is_empty() && quasis.len() == 1 && quasis[0].raw.is_empty() {
        return "\"\"".to_string();
    }
    let mut result = String::from("`");
    for (i, quasi) in quasis.iter().enumerate() {
        result.push_str(&quasi.raw);
        if i < subexprs.len() {
            result.push_str("${");
            result.push_str(&codegen_place_to_expression(cx, &subexprs[i]));
            result.push('}');
        }
    }
    result.push('`');
    result
}

// ---- Operator helpers ----

fn operator_to_str(op: &BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Add => "+",
        BinaryOperator::Sub => "-",
        BinaryOperator::Mul => "*",
        BinaryOperator::Div => "/",
        BinaryOperator::Mod => "%",
        BinaryOperator::Exp => "**",
        BinaryOperator::StrictEq => "===",
        BinaryOperator::StrictNotEq => "!==",
        BinaryOperator::Eq => "==",
        BinaryOperator::NotEq => "!=",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::BitAnd => "&",
        BinaryOperator::BitOr => "|",
        BinaryOperator::BitXor => "^",
        BinaryOperator::LShift => "<<",
        BinaryOperator::RShift => ">>",
        BinaryOperator::URShift => ">>>",
        BinaryOperator::In => "in",
        BinaryOperator::InstanceOf => "instanceof",
    }
}

fn unary_operator_to_str(op: &UnaryOperator) -> &'static str {
    match op {
        UnaryOperator::Minus => "-",
        UnaryOperator::Plus => "+",
        UnaryOperator::Not => "!",
        UnaryOperator::BitNot => "~",
        UnaryOperator::TypeOf => "typeof",
        UnaryOperator::Void => "void",
    }
}

fn update_op_to_str(op: &UpdateOperator) -> &'static str {
    match op {
        UpdateOperator::Increment => "++",
        UpdateOperator::Decrement => "--",
    }
}

/// Format a property access expression, using dot notation for string properties
/// and bracket notation for numeric properties. Supports optional chaining.
fn format_property_access(obj: &str, prop: &PropertyLiteral, optional: bool) -> String {
    let stripped = strip_redundant_member_object_parens(obj);
    let obj = if (stripped.contains(" as ") || stripped.contains(" satisfies "))
        && !stripped.starts_with('(')
    {
        format!("({})", stripped)
    } else {
        stripped
    };
    match prop {
        PropertyLiteral::String(s) => {
            if is_non_negative_integer_string(s) {
                if optional {
                    format!("{}?.[{}]", obj, s)
                } else {
                    format!("{}[{}]", obj, s)
                }
            } else if is_valid_js_identifier(s) {
                if optional {
                    format!("{}?.{}", obj, s)
                } else {
                    format!("{}.{}", obj, s)
                }
            } else if optional {
                format!("{}?.[\"{}\"]", obj, escape_string(s))
            } else {
                format!("{}[\"{}\"]", obj, escape_string(s))
            }
        }
        PropertyLiteral::Number(n) => {
            if optional {
                format!("{}?.[{}]", obj, n)
            } else {
                format!("{}[{}]", obj, n)
            }
        }
    }
}

fn strip_redundant_member_object_parens(obj: &str) -> String {
    let allocator = Allocator::default();
    let Some(ast::Expression::ParenthesizedExpression(parenthesized)) =
        parse_rendered_expression_ast(&allocator, obj)
    else {
        return obj.to_string();
    };
    let inner = parenthesized.unbox().expression;
    match inner {
        ast::Expression::CallExpression(_)
        | ast::Expression::ChainExpression(_)
        | ast::Expression::StaticMemberExpression(_)
        | ast::Expression::ComputedMemberExpression(_)
        | ast::Expression::PrivateFieldExpression(_) => codegen_expression_with_oxc(&inner),
        _ => obj.to_string(),
    }
}

// ---- String helpers ----

fn escape_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
        .replace('\u{0008}', "\\b")
        .replace('\u{000C}', "\\f")
}

/// Escape JSXText content. Unlike JS string literals, double quotes are valid
/// in JSX text and should not be escaped.
fn escape_jsx_text(s: &str) -> String {
    s.to_string()
}

// ---- JSX helpers ----

fn jsx_attr_needs_expression_container(escaped_inner: &str) -> bool {
    escaped_inner.contains('\\') || !escaped_inner.is_ascii()
}

fn jsx_text_needs_expression_container(escaped_inner: &str) -> bool {
    escaped_inner
        .chars()
        .any(|c| matches!(c, '<' | '>' | '&' | '{' | '}'))
}

fn unicode_escape_non_ascii(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            result.push(c);
        } else {
            let code = c as u32;
            if code > 0xFFFF {
                let high = ((code - 0x10000) >> 10) + 0xD800;
                let low = ((code - 0x10000) & 0x3FF) + 0xDC00;
                result.push_str(&format!("\\u{:04X}\\u{:04X}", high, low));
            } else {
                result.push_str(&format!("\\u{:04X}", code));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::{
        CodegenReactiveOptions, Context, FunctionExpressionType, apply_optional_to_rendered_expr,
        function_expr_as_declaration, function_expr_to_method_property,
        maybe_fill_for_header_initializer_from_update, reconstruct_for_init_declaration,
        render_function_expression_ast, render_object_method_ast, render_pattern_with_oxc,
        render_reactive_function_body_prologue_ast,
    };
    use crate::hir::types::{
        Argument, ArrayElement, ArrayPattern, DeclarationId, DependencyPathEntry, Effect,
        Identifier, IdentifierId, IdentifierName, MutableRange, ObjectProperty,
        ObjectPropertyKey, ObjectPropertyOrSpread, ObjectPropertyType, ObjectPattern, Pattern,
        Place, ReactiveScopeDependency, SourceLocation, Type,
    };
    use crate::reactive_scopes::codegen_reactive::CachePrologue;

    fn test_context() -> Context {
        let options = CodegenReactiveOptions::default();
        Context {
            next_cache_index: 0,
            declarations: HashSet::new(),
            runtime_emitted_declarations: HashSet::new(),
            temp: HashMap::new(),
            temp_by_identifier: HashMap::new(),
            object_methods: HashMap::new(),
            object_methods_store: Vec::new(),
            callback_deps: HashMap::new(),
            hook_callback_arg_decls: HashSet::new(),
            resolved_names: HashMap::new(),
            suppressed_temp_ids: Vec::new(),
            hook_call_by_decl: HashMap::new(),
            stable_ref_decls: HashSet::new(),
            stable_setter_decls: HashSet::new(),
            stable_effect_event_decls: HashSet::new(),
            multi_source_decls: HashSet::new(),
            decl_assignment_sources: HashMap::new(),
            stable_ref_names: HashSet::new(),
            unique_identifiers: HashSet::new(),
            fbt_operands: HashSet::new(),
            synthesized_names: HashMap::new(),
            next_temp_index: 0,
            temp_remap: HashMap::new(),
            declared_names: HashSet::new(),
            captured_in_child_functions: HashSet::new(),
            mutable_captured_in_child_functions: HashSet::new(),
            reassigned_decls: HashSet::new(),
            read_declarations: HashSet::new(),
            inline_primitive_literals: HashMap::new(),
            capturable_primitive_literals: HashMap::new(),
            non_local_binding_decls: HashSet::new(),
            disable_memoization_features: options.disable_memoization_features,
            disable_memoization_for_debugging: options.disable_memoization_for_debugging,
            enable_change_variable_codegen: options.enable_change_variable_codegen,
            emit_hook_guards: options.enable_emit_hook_guards,
            enable_change_detection_for_debugging: options.enable_change_detection_for_debugging,
            enable_name_anonymous_functions: options.enable_name_anonymous_functions,
            needs_structural_check_import: false,
            function_name: "<test>".to_string(),
            param_display_names: HashMap::new(),
            reserved_child_decl_names: HashSet::new(),
            block_scope_declared_temp_names: Vec::new(),
            declaration_name_overrides: HashMap::new(),
            used_declaration_names: HashSet::new(),
            preferred_decl_names: HashMap::new(),
            codegen_error: None,
            emitted_optional_dep_reads: HashSet::new(),
            pending_manual_memo_reads: HashSet::new(),
            manual_memo_root_decls: HashSet::new(),
            manual_memo_dep_roots_by_id: HashMap::new(),
            manual_memo_dep_roots_by_decl: HashMap::new(),
            pruned_manual_memo_decls: HashSet::new(),
            stable_zero_dep_decls: HashSet::new(),
            scope_dependency_decls: HashSet::new(),
            scope_dependency_overrides: HashMap::new(),
            function_decl_decls: HashSet::new(),
            jsx_only_component_tag_decls: HashSet::new(),
            inline_identifier_aliases: HashMap::new(),
            elided_named_declarations: HashSet::new(),
            preserve_loop_header_inits: false,
            block_scope_output_names: Vec::new(),
        }
    }

    fn named_place(id: u32, declaration_id: u32, name: &str) -> Place {
        Place {
            identifier: Identifier {
                id: IdentifierId::new(id),
                declaration_id: DeclarationId::new(declaration_id),
                name: Some(IdentifierName::Named(name.to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            effect: Effect::Read,
            reactive: false,
            loc: SourceLocation::Generated,
        }
    }

    #[test]
    fn renders_named_function_expression_via_ast() {
        let rendered = render_function_expression_ast(
            &[Argument::Place(named_place(0, 0, "value"))],
            &["value".to_string()],
            "return value;",
            &[],
            false,
            false,
            Some("useFoo"),
            &FunctionExpressionType::FunctionExpression,
            None,
        )
        .expect("expected function expression");

        assert!(rendered.contains("function useFoo(value)"));
        assert!(rendered.contains("return value;"));
    }

    #[test]
    fn renders_concise_arrow_function_via_ast() {
        let rendered = render_function_expression_ast(
            &[Argument::Place(named_place(0, 0, "value"))],
            &["value".to_string()],
            "return value;",
            &[],
            false,
            false,
            None,
            &FunctionExpressionType::ArrowFunctionExpression,
            None,
        )
        .expect("expected arrow expression");

        assert!(rendered.contains("=>"));
        assert!(!rendered.contains("return value;"));
    }

    #[test]
    fn converts_function_expression_to_declaration_via_ast() {
        let declaration =
            function_expr_as_declaration("useFoo", "async function(value) {\n  return value;\n}")
                .expect("expected function declaration");

        assert!(declaration.starts_with("async function useFoo(value)"));
        assert!(declaration.contains("return value;"));
    }

    #[test]
    fn renders_object_method_via_ast() {
        let rendered = render_object_method_ast(
            &ObjectPropertyKey::Identifier("run".to_string()),
            None,
            &[Argument::Place(named_place(0, 0, "value"))],
            &["value".to_string()],
            "return value;",
            &[],
            false,
            false,
        )
        .expect("expected object method");

        assert!(rendered.contains("run(value)"));
        assert!(rendered.contains("return value;"));
    }

    #[test]
    fn converts_function_expression_to_method_property_via_ast() {
        let rendered = function_expr_to_method_property(
            &ObjectPropertyKey::Identifier("callback".to_string()),
            None,
            "async function(value) {\n  return value;\n}",
        )
        .expect("expected method property");

        assert!(rendered.contains("async callback(value)"));
        assert!(rendered.contains("return value;"));
    }

    #[test]
    fn reconstructs_for_init_declaration_via_ast() {
        let rendered = reconstruct_for_init_declaration("let value;\nvalue = 1;")
            .expect("expected reconstructed header");

        assert_eq!(rendered, "let value = 1");
    }

    #[test]
    fn fills_for_header_initializer_via_ast() {
        let rendered = maybe_fill_for_header_initializer_from_update("const index", "0")
            .expect("expected filled header");

        assert_eq!(rendered, "const index = 0");
    }

    #[test]
    fn renders_function_body_directives_via_ast() {
        let rendered = render_reactive_function_body_prologue_ast(Some(&["worklet".to_string()]), None)
            .expect("expected prologue");

        assert_eq!(rendered.trim(), "\"worklet\";");
    }

    #[test]
    fn renders_function_body_directives_and_cache_via_ast() {
        let rendered = render_reactive_function_body_prologue_ast(
            Some(&["use strict".to_string()]),
            Some(&CachePrologue {
                binding_name: "$".to_string(),
                size: 1,
                fast_refresh: None,
            }),
        )
        .expect("expected prologue");

        assert!(rendered.starts_with("\"use strict\";"));
        assert!(rendered.contains("const $ = _c(1);"));
    }

    #[test]
    fn optionalizes_rendered_call_via_ast() {
        let rendered =
            apply_optional_to_rendered_expr("foo(bar)", true).expect("expected optional call");

        assert_eq!(rendered, "foo?.(bar)");
    }

    #[test]
    fn optionalizes_rendered_member_via_ast() {
        let rendered =
            apply_optional_to_rendered_expr("foo.bar", true).expect("expected optional member");

        assert_eq!(rendered, "foo?.bar");
    }

    #[test]
    fn renders_dependency_chain_via_ast() {
        let dep = ReactiveScopeDependency {
            identifier: Identifier {
                id: IdentifierId::new(0),
                declaration_id: DeclarationId::new(0),
                name: Some(IdentifierName::Named("foo".to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            path: vec![DependencyPathEntry {
                property: "bar".to_string(),
                optional: true,
            }],
        };

        let rendered =
            super::render_dependency_expression_ast("foo", &dep).expect("expected dependency");

        assert_eq!(rendered, "foo?.bar");
    }

    #[test]
    fn renders_computed_dependency_chain_via_ast() {
        let dep = ReactiveScopeDependency {
            identifier: Identifier {
                id: IdentifierId::new(0),
                declaration_id: DeclarationId::new(0),
                name: Some(IdentifierName::Named("foo".to_string())),
                mutable_range: MutableRange::default(),
                scope: None,
                type_: Type::Poly,
                loc: SourceLocation::Generated,
            },
            path: vec![DependencyPathEntry {
                property: "bad-key".to_string(),
                optional: false,
            }],
        };

        let rendered =
            super::render_dependency_expression_ast("foo", &dep).expect("expected dependency");

        assert_eq!(rendered, "foo[\"bad-key\"]");
    }

    #[test]
    fn renders_array_pattern_via_ast() {
        let mut cx = test_context();
        let pattern = Pattern::Array(ArrayPattern {
            items: vec![
                ArrayElement::Place(named_place(0, 0, "a")),
                ArrayElement::Hole,
                ArrayElement::Spread(named_place(1, 1, "rest")),
            ],
        });

        let rendered = render_pattern_with_oxc(&mut cx, &pattern).expect("expected pattern");

        assert_eq!(rendered, "[a, , ...rest]");
    }

    #[test]
    fn renders_object_pattern_via_ast() {
        let mut cx = test_context();
        let pattern = Pattern::Object(ObjectPattern {
            properties: vec![
                ObjectPropertyOrSpread::Property(ObjectProperty {
                        key: ObjectPropertyKey::Identifier("value".to_string()),
                        place: named_place(0, 0, "value"),
                        type_: ObjectPropertyType::Property,
                    }),
                ObjectPropertyOrSpread::Spread(named_place(1, 1, "rest")),
            ],
        });

        let rendered = render_pattern_with_oxc(&mut cx, &pattern).expect("expected pattern");

        assert_eq!(rendered, "{ value, ...rest }");
    }
}
