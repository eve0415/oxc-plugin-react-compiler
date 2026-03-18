//! Direct ReactiveFunction → OXC AST codegen.
//!
//! Port of `CodegenReactiveFunction.ts` from upstream. Walks the tree-shaped
//! ReactiveFunction and emits OXC AST statements with memoization cache guards,
//! replacing the intermediate string codegen + GeneratedBodyShape pipeline.

use std::collections::{HashMap, HashSet};

use oxc_allocator::{Allocator, CloneIn};
use oxc_ast::{AstBuilder, NONE, ast};
use oxc_parser::Parser;
use oxc_span::SPAN;
use oxc_syntax::identifier::is_identifier_name;
use oxc_syntax::number::NumberBase;
use oxc_syntax::operator::{AssignmentOperator, BinaryOperator, LogicalOperator};

use crate::error::CompilerError;
use crate::hir::types::*;

thread_local! {
    /// Captures unnamed identifier invariant errors from the free `identifier_name()`
    /// function which doesn't have access to CodegenContext.
    static CODEGEN_UNNAMED_ERROR: std::cell::RefCell<Option<CompilerError>> =
        const { std::cell::RefCell::new(None) };
}

// ---------------------------------------------------------------------------
// Cache prologue types (shared between codegen_ast and module_emitter)
// ---------------------------------------------------------------------------

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

/// Sentinel value for uninitialized cache slots.
pub const MEMO_CACHE_SENTINEL: &str = "react.memo_cache_sentinel";
/// Sentinel value for early return detection.
pub const EARLY_RETURN_SENTINEL: &str = "react.early_return_sentinel";
/// Marker function name for Flow type casts; module_emitter restores `(value: Type)` syntax.
const FLOW_CAST_MARKER_HELPER: &str = "__REACT_COMPILER_FLOW_CAST__";
/// Internal blank line marker used by module_emitter for formatting.
pub(crate) const INTERNAL_BLANK_LINE_MARKER: &str = "__REACT_COMPILER_INTERNAL_BLANK_LINE_MARKER__";
/// Hook guard push operation constant.
pub(crate) const HOOK_GUARD_PUSH: u8 = 0;
/// Hook guard pop operation constant.
pub(crate) const HOOK_GUARD_POP: u8 = 1;

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

pub struct CodegenOptions {
    pub enable_change_variable_codegen: bool,
    pub enable_emit_hook_guards: bool,
    pub enable_change_detection_for_debugging: bool,
    pub enable_reset_cache_on_source_file_changes: bool,
    pub fast_refresh_source_hash: Option<String>,
    pub disable_memoization_features: bool,
    pub disable_memoization_for_debugging: bool,
    pub fbt_operands: HashSet<IdentifierId>,
    /// Cache binding name override (e.g., "$0" when "$" is renamed to avoid conflicts).
    pub cache_binding_name: Option<String>,
    /// Set of all used identifier names (from rename_variables). Used to resolve
    /// temp naming collisions when the same Promoted name appears in different scopes.
    pub unique_identifiers: HashSet<String>,
    /// DeclarationId → name overrides for outlined function parameters whose
    /// identifiers are unnamed in the ReactiveFunction IR.
    pub param_name_overrides: HashMap<DeclarationId, String>,
    /// Wrap anonymous function expressions with generated name hints.
    pub enable_name_anonymous_functions: bool,
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

pub struct CodegenFunctionResult<'a> {
    pub body: oxc_allocator::Vec<'a, ast::Statement<'a>>,
    pub cache_size: u32,
    pub needs_cache_import: bool,
    pub param_names: Vec<String>,
    pub needs_hook_guards: bool,
    pub needs_function_hook_guard_wrapper: bool,
    pub needs_structural_check_import: bool,
    pub cache_prologue: Option<CachePrologue>,
    pub error: Option<CompilerError>,
}

/// Metadata-only result from codegen_ast (no AST body, no lifetime).
/// Used by the pipeline to extract codegen metadata without keeping the AST.
#[derive(Clone)]
pub struct CodegenMetadata {
    pub cache_size: u32,
    pub needs_cache_import: bool,
    pub param_names: Vec<String>,
    pub needs_hook_guards: bool,
    pub needs_function_hook_guard_wrapper: bool,
    pub needs_structural_check_import: bool,
    pub cache_prologue: Option<CachePrologue>,
    pub error: Option<CompilerError>,
}

impl<'a> CodegenFunctionResult<'a> {
    /// Extract metadata without the AST body.
    pub fn metadata(&self) -> CodegenMetadata {
        CodegenMetadata {
            cache_size: self.cache_size,
            needs_cache_import: self.needs_cache_import,
            param_names: self.param_names.clone(),
            needs_hook_guards: self.needs_hook_guards,
            needs_function_hook_guard_wrapper: self.needs_function_hook_guard_wrapper,
            needs_structural_check_import: self.needs_structural_check_import,
            cache_prologue: self.cache_prologue.clone(),
            error: self.error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct CodegenContext<'a> {
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    /// Identifiers that have been declared (to avoid re-declaring), tracked by IdentifierId.
    declared: HashSet<IdentifierId>,
    /// Declaration IDs that have been declared (to avoid duplicates from different SSA ids).
    declared_decl_ids: HashSet<DeclarationId>,
    /// Variable names that have been declared (to avoid duplicate `let` for same name).
    declared_names: HashSet<String>,
    /// Temp expressions: keyed by DeclarationId (shared across SSA uses) to match
    /// upstream's `cx.temp.set(declarationId, value)` / `cx.temp.get(declarationId)`.
    temps: HashMap<DeclarationId, Option<ast::Expression<'a>>>,
    /// Next cache slot index.
    next_cache_index: u32,
    /// Cache binding name (e.g., "$").
    cache_binding: String,
    /// Whether we emitted hook guard calls.
    emitted_hook_guards: bool,
    /// Whether we need the function-level hook guard wrapper.
    needs_function_hook_guard_wrapper: bool,
    /// Whether we emitted structural check calls.
    needs_structural_check: bool,
    /// Options controlling codegen behavior.
    options: CodegenOptions,
    /// Function name for structural check diagnostics.
    #[allow(dead_code)]
    fn_name: String,
    /// Pre-computed DeclarationId → display name from string codegen.
    /// Only used to resolve identifier references — does NOT affect temp inlining.
    name_overrides: HashMap<DeclarationId, String>,
    /// DeclarationId → resolved name, populated as identifiers are first encountered.
    /// Used as fallback when a Place reference has no name set.
    decl_names: HashMap<DeclarationId, String>,
    /// Stack of name scopes for block-scoped declaration tracking.
    /// When entering an If/For/While block, push a new frame.
    /// When leaving, pop and remove those names from declared_names.
    name_scope_stack: Vec<Vec<String>>,
    /// DeclarationIds for optional chain deps that were pre-extracted to temp variables.
    /// Prevents cx.temps from being overwritten when the corresponding PropertyLoad
    /// is processed during body codegen.
    extracted_optional_deps: HashSet<DeclarationId>,
    /// DeclarationIds that should be force-inlined from the temp map even though
    /// they have named identifiers. Set by the destructure+call fusion pre-scan.
    force_inline_decls: HashSet<DeclarationId>,
    /// Error encountered during codegen (e.g., unnamed identifier invariant).
    codegen_error: Option<CompilerError>,
    /// DeclarationIds corresponding to JSXText instruction values, used to
    /// distinguish JSXText from StringLiteral when lowering JSX children.
    jsx_text_decl_ids: HashSet<DeclarationId>,
}

impl<'a> CodegenContext<'a> {
    /// Resolve an identifier's display name, checking decl_names overrides first.
    /// Records an invariant error if the identifier is unnamed, matching upstream
    /// `convertIdentifier()` invariant (CompilerError.ts:2922-2939).
    fn resolve_identifier_name(&mut self, identifier: &Identifier) -> String {
        if let Some(shifted) = self.decl_names.get(&identifier.declaration_id) {
            return shifted.clone();
        }
        // Upstream invariant: all identifiers reaching codegen should be named
        if identifier.name.is_none() && self.codegen_error.is_none() {
            self.codegen_error = Some(CompilerError::invariant(
                "Expected temporaries to be promoted to named identifiers in an earlier pass",
                Some(format!("identifier {} is unnamed", identifier.id.0)),
            ));
        }
        identifier_name(identifier)
    }

    /// Generate a unique name that doesn't collide with existing identifiers.
    /// Mirrors upstream's `Context.synthesizeName()` — checks unique_identifiers
    /// and appends an incrementing suffix until collision-free.
    fn synthesize_name(&mut self, base: &str) -> String {
        let mut name = base.to_string();
        let mut index = 0u32;
        while self.options.unique_identifiers.contains(&name) || self.declared_names.contains(&name)
        {
            name = format!("{base}{index}");
            index += 1;
        }
        self.options.unique_identifiers.insert(name.clone());
        self.declared_names.insert(name.clone());
        name
    }

    fn alloc_cache_slot(&mut self) -> u32 {
        let slot = self.next_cache_index;
        self.next_cache_index += 1;
        slot
    }

    fn cache_access(&self, index: u32) -> ast::Expression<'a> {
        ast::Expression::from(
            self.builder.member_expression_computed(
                SPAN,
                self.builder
                    .expression_identifier(SPAN, self.builder.ident(&self.cache_binding)),
                self.builder.expression_numeric_literal(
                    SPAN,
                    f64::from(index),
                    None,
                    NumberBase::Decimal,
                ),
                false,
            ),
        )
    }

    fn cache_assign(&self, index: u32, value: ast::Expression<'a>) -> ast::Expression<'a> {
        self.builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                self.builder.member_expression_computed(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident(&self.cache_binding)),
                    self.builder.expression_numeric_literal(
                        SPAN,
                        f64::from(index),
                        None,
                        NumberBase::Decimal,
                    ),
                    false,
                ),
            )),
            value,
        )
    }

    fn sentinel_expr(&self) -> ast::Expression<'a> {
        self.builder.expression_call(
            SPAN,
            ast::Expression::from(
                self.builder.member_expression_static(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident("Symbol")),
                    self.builder.identifier_name(SPAN, "for"),
                    false,
                ),
            ),
            NONE,
            self.builder
                .vec1(ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(MEMO_CACHE_SENTINEL),
                    None,
                ))),
            false,
        )
    }

    fn early_return_sentinel_expr(&self) -> ast::Expression<'a> {
        self.builder.expression_call(
            SPAN,
            ast::Expression::from(
                self.builder.member_expression_static(
                    SPAN,
                    self.builder
                        .expression_identifier(SPAN, self.builder.ident("Symbol")),
                    self.builder.identifier_name(SPAN, "for"),
                    false,
                ),
            ),
            NONE,
            self.builder
                .vec1(ast::Argument::from(self.builder.expression_string_literal(
                    SPAN,
                    self.builder.atom(EARLY_RETURN_SENTINEL),
                    None,
                ))),
            false,
        )
    }

    fn ident_expr(&self, name: &str) -> ast::Expression<'a> {
        self.builder
            .expression_identifier(SPAN, self.builder.ident(name))
    }

    /// Push a new name scope frame (entering a block like if/for/while).
    fn push_name_scope(&mut self) {
        self.name_scope_stack.push(Vec::new());
    }

    /// Pop a name scope frame (leaving a block). Names declared in this
    /// frame are removed from declared_names so they can be re-declared
    /// in sibling scopes without collision.
    fn pop_name_scope(&mut self) {
        if let Some(frame) = self.name_scope_stack.pop() {
            for name in frame {
                self.declared_names.remove(&name);
            }
        }
    }

    /// Register a name in the current scope frame (for later removal on pop).
    fn register_scoped_name(&mut self, name: &str) {
        if let Some(frame) = self.name_scope_stack.last_mut() {
            frame.push(name.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn codegen_reactive_function<'a>(
    builder: AstBuilder<'a>,
    allocator: &'a Allocator,
    func: &ReactiveFunction,
    mut options: CodegenOptions,
) -> CodegenFunctionResult<'a> {
    let cache_binding = options.cache_binding_name.clone().unwrap_or_else(|| {
        // Synthesize unique cache binding name: "$", "$0", "$1", ...
        let mut name = "$".to_string();
        let mut suffix = 0u32;
        while options.unique_identifiers.contains(&name) {
            name = format!("${suffix}");
            suffix += 1;
        }
        name
    });
    let initial_decl_names = std::mem::take(&mut options.param_name_overrides);
    let disable_memoization_features = options.disable_memoization_features;

    // Clear thread-local error from previous codegen runs
    CODEGEN_UNNAMED_ERROR.with(|slot| *slot.borrow_mut() = None);

    let mut cx = CodegenContext {
        builder,
        allocator,
        declared: HashSet::new(),
        declared_decl_ids: HashSet::new(),
        declared_names: HashSet::new(),
        temps: HashMap::new(),
        next_cache_index: 0,
        cache_binding: cache_binding.clone(),
        emitted_hook_guards: false,
        needs_function_hook_guard_wrapper: false,
        needs_structural_check: false,
        options,
        name_overrides: HashMap::new(),
        decl_names: initial_decl_names,
        name_scope_stack: Vec::new(),
        fn_name: func.id.clone().unwrap_or_default(),
        extracted_optional_deps: HashSet::new(),
        force_inline_decls: HashSet::new(),
        codegen_error: None,
        jsx_text_decl_ids: HashSet::new(),
    };

    // Pre-collect preferred names: scan body for named references to
    // declaration_ids that may be unnamed in the param list.
    let preferred_names = collect_preferred_decl_names(&func.body);

    // Collect param names (matching string codegen's identifier_name_with_cx behavior).
    // Unnamed params check preferred_names first, then fall back to "tN".
    // In bailout mode (disable_memoization_features), temp param names are overridden
    // to "tN" where N is the param index (matching string codegen's behavior).
    let param_names: Vec<String> = func
        .params
        .iter()
        .enumerate()
        .map(|(param_index, arg)| {
            let ident = match arg {
                Argument::Place(place) => &place.identifier,
                Argument::Spread(place) => &place.identifier,
            };
            let raw_name = if let Some(name) = ident.name.as_ref() {
                name.value().to_string()
            } else if let Some(preferred) = preferred_names.get(&ident.declaration_id) {
                preferred.clone()
            } else {
                // Unnamed param — record invariant error matching upstream
                // convertIdentifier(). Skip in bailout mode where unnamed
                // params are expected.
                if !disable_memoization_features {
                    CODEGEN_UNNAMED_ERROR.with(|slot| {
                        let mut slot = slot.borrow_mut();
                        if slot.is_none() {
                            *slot = Some(CompilerError::invariant(
                                "Expected temporaries to be promoted to named identifiers in an earlier pass",
                                Some(format!("identifier {} is unnamed", ident.id.0)),
                            ));
                        }
                    });
                }
                format!("t{}", ident.id.0)
            };
            // In bailout mode, override temp names to sequential "tN".
            if disable_memoization_features && is_codegen_temp(&raw_name) {
                format!("t{param_index}")
            } else {
                raw_name
            }
        })
        .collect();

    // Mark params as declared and record their names for DeclarationId lookups.
    for arg in &func.params {
        let (id, decl_id, name) = match arg {
            Argument::Place(place) => (
                place.identifier.id,
                place.identifier.declaration_id,
                &place.identifier.name,
            ),
            Argument::Spread(place) => (
                place.identifier.id,
                place.identifier.declaration_id,
                &place.identifier.name,
            ),
        };
        cx.declared.insert(id);
        if let Some(n) = name {
            let name_str = n.value().to_string();
            cx.declared_names.insert(name_str.clone());
            cx.decl_names.insert(decl_id, name_str);
        }
    }

    // Allocate fast-refresh slot BEFORE body codegen so it gets slot 0
    // (matching upstream behavior in CodegenReactiveFunction.ts).
    let fast_refresh_slot = if cx.options.enable_reset_cache_on_source_file_changes
        && cx.options.fast_refresh_source_hash.is_some()
    {
        Some(cx.alloc_cache_slot())
    } else {
        None
    };

    let mut body_stmts = codegen_block(&mut cx, &func.body);

    // Strip trailing void return (`return;` or `return undefined`).
    if let Some(ast::Statement::ReturnStatement(ret)) = body_stmts.last() {
        let is_void = ret.argument.is_none()
            || matches!(
                &ret.argument,
                Some(ast::Expression::Identifier(id)) if id.name == "undefined"
            );
        if is_void {
            body_stmts.pop();
        }
    }

    let cache_size = cx.next_cache_index;

    // Build cache prologue if needed.
    // When disable_memoization_features is true (bailout-retry mode),
    // no cache import is needed — the function runs without memoization.
    let cache_prologue = if cache_size > 0 && !cx.options.disable_memoization_features {
        let fast_refresh = fast_refresh_slot.and_then(|slot| {
            cx.options
                .fast_refresh_source_hash
                .as_ref()
                .map(|hash| FastRefreshPrologue {
                    cache_index: slot,
                    hash: hash.clone(),
                    index_binding_name: "$i".to_string(),
                })
        });
        Some(CachePrologue {
            binding_name: cache_binding,
            size: cache_size,
            fast_refresh,
        })
    } else {
        None
    };

    let needs_cache_import = cache_prologue.is_some();

    let body = builder.vec_from_iter(body_stmts);

    CodegenFunctionResult {
        body,
        cache_size,
        needs_cache_import,
        param_names,
        needs_hook_guards: cx.emitted_hook_guards,
        needs_function_hook_guard_wrapper: cx.needs_function_hook_guard_wrapper,
        needs_structural_check_import: cx.needs_structural_check,
        cache_prologue,
        error: cx
            .codegen_error
            .or_else(|| CODEGEN_UNNAMED_ERROR.with(|slot| slot.borrow_mut().take())),
    }
}

// ---------------------------------------------------------------------------
// Block dispatcher
// ---------------------------------------------------------------------------

fn codegen_block<'a>(
    cx: &mut CodegenContext<'a>,
    block: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    codegen_block_no_reset(cx, block)
}

fn codegen_block_no_reset<'a>(
    cx: &mut CodegenContext<'a>,
    block: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    // Pre-scan: mark catch bindings from Try terminals as declared so that
    // preceding DeclareLocal instructions don't emit `let` for them.
    for stmt in block.iter() {
        if let ReactiveStatement::Terminal(term) = stmt
            && let ReactiveTerminal::Try {
                handler_binding, ..
            } = &term.terminal
            && let Some(binding) = handler_binding
        {
            cx.declared.insert(binding.identifier.id);
            cx.declared_decl_ids
                .insert(binding.identifier.declaration_id);
            if let Some(n) = &binding.identifier.name {
                cx.declared_names.insert(n.value().to_string());
            }
        }
    }

    let mut stmts = Vec::new();
    let block_vec: Vec<&ReactiveStatement> = block.iter().collect();

    // Pre-scan: detect Destructure(Reassign) + Call fusion patterns and mark
    // promoted-temp instructions that should be suppressed (inlined instead).
    let fuse_suppress = pre_scan_destructure_call_fusion(&block_vec);

    let mut i = 0;

    while i < block_vec.len() {
        match block_vec[i] {
            ReactiveStatement::Instruction(instr) => {
                // Skip instructions suppressed by the fusion pre-scan.
                if fuse_suppress.contains(&i) {
                    // Force-inline this instruction's value into the temp map
                    // and mark as force-inlineable so codegen_place uses it.
                    if let Some(lvalue) = &instr.lvalue {
                        let decl_id = lvalue.identifier.declaration_id;
                        if let Some(expr) = codegen_instruction_value(cx, &instr.value) {
                            cx.temps.insert(decl_id, Some(expr));
                            cx.force_inline_decls.insert(decl_id);
                        }
                    }
                    i += 1;
                    continue;
                }
                // Fusion: Destructure(Reassign) + MethodCall/CallExpression
                // Pattern: `[x] = rhs; obj.foo(rhs)` → `obj.foo(([x] = rhs))`
                if let Some(fused) = try_fuse_destructure_into_call(cx, instr, block_vec.get(i + 1))
                {
                    stmts.push(fused);
                    i += 2;
                    continue;
                }
                if let Some(s) = codegen_instruction(cx, instr) {
                    stmts.push(s);
                }
            }
            ReactiveStatement::Terminal(term_stmt) => {
                let label = term_stmt.label.as_ref();
                let mut terminal_stmts = codegen_terminal(cx, &term_stmt.terminal);

                // Attach label if needed.
                if let Some(label) = label
                    && !label.implicit
                    && !terminal_stmts.is_empty()
                {
                    let first = &terminal_stmts[0];
                    // If the first statement is a declaration (const/let/var),
                    // wrap all statements in a block to avoid invalid labeled
                    // declarations (e.g., `bb0: const x = ...` is a syntax error).
                    let needs_block = matches!(
                        first,
                        ast::Statement::VariableDeclaration(_)
                            | ast::Statement::FunctionDeclaration(_)
                            | ast::Statement::ClassDeclaration(_)
                    );
                    let body = if needs_block {
                        cx.builder.statement_block(
                            SPAN,
                            cx.builder.vec_from_iter(terminal_stmts.drain(..)),
                        )
                    } else {
                        terminal_stmts.remove(0)
                    };
                    let labeled = cx.builder.statement_labeled(
                        SPAN,
                        cx.builder
                            .label_identifier(SPAN, crate_label_name(label.id)),
                        body,
                    );
                    terminal_stmts.insert(0, labeled);
                }
                stmts.extend(terminal_stmts);
            }
            ReactiveStatement::Scope(scope_block) => {
                let scope_stmts =
                    codegen_reactive_scope(cx, &scope_block.scope, &scope_block.instructions);
                stmts.extend(scope_stmts);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                // Pruned scopes: emit instructions without memoization wrapper.
                let inner = codegen_block(cx, &pruned.instructions);
                stmts.extend(inner);
            }
        }
        i += 1;
    }

    stmts
}

/// Pre-scan a block to find Destructure(Reassign) + Call fusion patterns and
/// return the set of instruction indices whose declarations should be suppressed
/// (force-inlined) so that the fusion emits a single compound expression.
///
/// The pattern we detect (working backwards from a Destructure + Call pair):
///   [i]   Destructure { value: V, kind: Reassign } (no lvalue)
///   [i+1] MethodCall/CallExpression { args containing V } (no lvalue)
/// Any promoted-temp instructions preceding [i] whose lvalue is only used
/// within the Destructure+Call pair are suppressed.
fn pre_scan_destructure_call_fusion(
    block: &[&ReactiveStatement],
) -> std::collections::HashSet<usize> {
    let mut suppress: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for i in 0..block.len().saturating_sub(1) {
        let ReactiveStatement::Instruction(destr_instr) = block[i] else {
            continue;
        };
        if destr_instr.lvalue.is_some() {
            continue;
        }
        let InstructionValue::Destructure { lvalue, value, .. } = &destr_instr.value else {
            continue;
        };
        if lvalue.kind != InstructionKind::Reassign {
            continue;
        }
        let ReactiveStatement::Instruction(call_instr) = block[i + 1] else {
            continue;
        };
        if call_instr.lvalue.is_some() {
            continue;
        }
        let call_args = match &call_instr.value {
            InstructionValue::MethodCall { args, .. } => args,
            InstructionValue::CallExpression { args, .. } => args,
            _ => continue,
        };
        let value_decl_id = value.identifier.declaration_id;
        let has_matching_arg = call_args.iter().any(
            |arg| matches!(arg, Argument::Place(p) if p.identifier.declaration_id == value_decl_id),
        );
        if !has_matching_arg {
            continue;
        }

        // Collect all declaration_ids referenced by the Destructure + Call pair.
        let mut pair_refs: std::collections::HashSet<DeclarationId> =
            std::collections::HashSet::new();
        collect_value_refs(&destr_instr.value, &mut pair_refs);
        collect_value_refs(&call_instr.value, &mut pair_refs);

        // Walk backwards from i to find promoted-temp instructions to suppress.
        // Only suppress instructions that:
        // 1. Have a named lvalue (promoted temp)
        // 2. The lvalue's decl_id is referenced by the pair
        // 3. The lvalue's decl_id is NOT referenced by anything outside the pair
        //    (in the rest of the block)
        let mut outside_refs: std::collections::HashSet<DeclarationId> =
            std::collections::HashSet::new();
        for (j, stmt) in block.iter().enumerate() {
            if j == i || j == i + 1 {
                continue;
            }
            if let ReactiveStatement::Instruction(other_instr) = stmt {
                // Skip unnamed temp instructions — they only pass through values
                // and don't constitute genuine "outside" references.
                if let Some(lv) = &other_instr.lvalue
                    && lv.identifier.name.is_none()
                {
                    continue;
                }
                collect_value_refs(&other_instr.value, &mut outside_refs);
            }
        }

        for j in (0..i).rev() {
            let ReactiveStatement::Instruction(prev_instr) = block[j] else {
                break;
            };
            let Some(prev_lv) = &prev_instr.lvalue else {
                break;
            };
            let prev_decl_id = prev_lv.identifier.declaration_id;
            // Skip unnamed temps — they'll be naturally inlined by codegen.
            if prev_lv.identifier.name.is_none() {
                continue;
            }
            if !pair_refs.contains(&prev_decl_id) {
                break;
            }
            if outside_refs.contains(&prev_decl_id) {
                break;
            }
            // This instruction's value should be inlined.
            suppress.insert(j);
        }
    }

    suppress
}

fn collect_value_refs(
    value: &InstructionValue,
    refs: &mut std::collections::HashSet<DeclarationId>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            refs.insert(place.identifier.declaration_id);
        }
        InstructionValue::Destructure { value: v, .. } => {
            refs.insert(v.identifier.declaration_id);
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            refs.insert(receiver.identifier.declaration_id);
            refs.insert(property.identifier.declaration_id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        refs.insert(p.identifier.declaration_id);
                    }
                }
            }
        }
        InstructionValue::CallExpression { callee, args, .. } => {
            refs.insert(callee.identifier.declaration_id);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        refs.insert(p.identifier.declaration_id);
                    }
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. } => {
            refs.insert(object.identifier.declaration_id);
        }
        _ => {}
    }
}

/// Fuse a Destructure(Reassign) + MethodCall/CallExpression pair into a single
/// expression statement where the destructuring assignment becomes an argument.
///
/// Pattern detected:
///   Destructure { pattern: [x], value: rhs_place, kind: Reassign } (no lvalue)
///   MethodCall { receiver, property, args: [.., rhs_place, ..] }   (no lvalue)
///
/// Emitted:  receiver.property(([x] = rhs_expr))
fn try_fuse_destructure_into_call<'a>(
    cx: &mut CodegenContext<'a>,
    destructure_instr: &ReactiveInstruction,
    next_stmt: Option<&&ReactiveStatement>,
) -> Option<ast::Statement<'a>> {
    // 1. Current instruction must be Destructure(Reassign) with no lvalue.
    if destructure_instr.lvalue.is_some() {
        return None;
    }
    let InstructionValue::Destructure { lvalue, value, .. } = &destructure_instr.value else {
        return None;
    };
    if lvalue.kind != InstructionKind::Reassign {
        return None;
    }

    // 2. Next statement must be an instruction with MethodCall or CallExpression, no lvalue.
    let ReactiveStatement::Instruction(call_instr) = next_stmt? else {
        return None;
    };
    if call_instr.lvalue.is_some() {
        return None;
    }

    let value_decl_id = value.identifier.declaration_id;

    // 3. Find which argument of the call uses the destructure's value place.
    let args = match &call_instr.value {
        InstructionValue::MethodCall { args, .. } => args,
        InstructionValue::CallExpression { args, .. } => args,
        _ => return None,
    };
    let assign_arg_index = args.iter().position(
        |arg| matches!(arg, Argument::Place(p) if p.identifier.declaration_id == value_decl_id),
    )?;

    // 4. Build the destructuring assignment expression: (pattern = rhs_expr).
    let rhs_expr = codegen_place(cx, value)?;
    let target = build_assignment_target_from_pattern(cx, &lvalue.pattern)?;
    let assign_expr = cx.builder.expression_parenthesized(
        SPAN,
        cx.builder
            .expression_assignment(SPAN, AssignmentOperator::Assign, target, rhs_expr),
    );

    // Helper: codegen a single argument, substituting at the fused index.
    let codegen_arg_or_fused = |cx: &mut CodegenContext<'a>,
                                idx: usize,
                                arg: &Argument,
                                assign: &ast::Expression<'a>|
     -> Option<ast::Argument<'a>> {
        if idx == assign_arg_index {
            Some(ast::Argument::from(assign.clone_in(cx.allocator)))
        } else {
            match arg {
                Argument::Place(place) => Some(ast::Argument::from(codegen_place(cx, place)?)),
                Argument::Spread(place) => Some(
                    cx.builder
                        .argument_spread_element(SPAN, codegen_place(cx, place)?),
                ),
            }
        }
    };

    // 5. Build the call expression, substituting the assignment expression for the argument.
    let call_expr = match &call_instr.value {
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional,
            call_optional,
            ..
        } => {
            let callee = codegen_method_call_callee(cx, receiver, property, *receiver_optional)?;
            let mut arg_exprs = cx.builder.vec();
            for (idx, arg) in args.iter().enumerate() {
                arg_exprs.push(codegen_arg_or_fused(cx, idx, arg, &assign_expr)?);
            }
            cx.builder
                .expression_call(SPAN, callee, NONE, arg_exprs, *call_optional)
        }
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => {
            let callee_expr = codegen_place(cx, callee)?;
            let mut arg_exprs = cx.builder.vec();
            for (idx, arg) in args.iter().enumerate() {
                arg_exprs.push(codegen_arg_or_fused(cx, idx, arg, &assign_expr)?);
            }
            cx.builder
                .expression_call(SPAN, callee_expr, NONE, arg_exprs, *optional)
        }
        _ => return None,
    };

    Some(cx.builder.statement_expression(SPAN, call_expr))
}

// ---------------------------------------------------------------------------
// Instruction codegen
// ---------------------------------------------------------------------------

fn codegen_instruction<'a>(
    cx: &mut CodegenContext<'a>,
    instr: &ReactiveInstruction,
) -> Option<ast::Statement<'a>> {
    // ── Statement-level variants (self-contained, use InstructionValue's lvalue) ──
    match &instr.value {
        InstructionValue::DeclareLocal { lvalue, .. }
        | InstructionValue::DeclareContext { lvalue, .. } => {
            return codegen_declare(cx, lvalue);
        }
        InstructionValue::StoreLocal { lvalue, value, .. } => {
            // Upstream: for StoreLocal/Reassign with a temp instruction-level
            // lvalue, inline the assignment expression into the temp map.
            if matches!(lvalue.kind, InstructionKind::Reassign)
                && let Some(outer_lv) = &instr.lvalue
                && is_temp_identifier(&outer_lv.identifier)
            {
                let rhs = codegen_place(cx, value)?;
                let assign_name = identifier_name(&lvalue.place.identifier);
                let assign_expr = cx.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        cx.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                cx.builder.ident(&assign_name),
                            ),
                    ),
                    rhs,
                );
                cx.temps
                    .insert(outer_lv.identifier.declaration_id, Some(assign_expr));
                return None;
            }
            return codegen_store(cx, lvalue, value);
        }
        InstructionValue::StoreContext { lvalue, value, .. } => {
            // Upstream: for StoreContext/Reassign with an instruction-level lvalue,
            // create the assignment expression and either inline it (unnamed temp)
            // or emit as `const t = w = expr` (promoted/named temp).
            if matches!(lvalue.kind, InstructionKind::Reassign)
                && let Some(outer_lv) = &instr.lvalue
            {
                let rhs = codegen_place(cx, value)?;
                let assign_name = identifier_name(&lvalue.place.identifier);
                let assign_expr = cx.builder.expression_assignment(
                    SPAN,
                    AssignmentOperator::Assign,
                    ast::AssignmentTarget::from(
                        cx.builder
                            .simple_assignment_target_assignment_target_identifier(
                                SPAN,
                                cx.builder.ident(&assign_name),
                            ),
                    ),
                    rhs,
                );
                if is_temp_identifier(&outer_lv.identifier) {
                    // Unnamed temp: store in temp map for inline substitution.
                    cx.temps
                        .insert(outer_lv.identifier.declaration_id, Some(assign_expr));
                    return None;
                }
                // Named/promoted temp: emit `const t = w = expr` declaration.
                let outer_name = cx.resolve_identifier_name(&outer_lv.identifier);
                cx.declared.insert(outer_lv.identifier.id);
                return Some(emit_var_decl_stmt_inner(
                    cx,
                    &outer_name,
                    ast::VariableDeclarationKind::Const,
                    Some(assign_expr),
                    Some(outer_lv.identifier.declaration_id),
                ));
            }
            return codegen_store(cx, lvalue, value);
        }
        InstructionValue::Destructure { lvalue, value, .. } => {
            return codegen_destructure(cx, lvalue, value);
        }
        InstructionValue::Debugger { .. } => {
            return Some(cx.builder.statement_debugger(SPAN));
        }
        InstructionValue::StartMemoize { .. } | InstructionValue::FinishMemoize { .. } => {
            return None;
        }
        _ => {}
    }

    // Track JSXText instructions for JSX child lowering distinction.
    if matches!(instr.value, InstructionValue::JSXText { .. })
        && let Some(lvalue) = &instr.lvalue
    {
        cx.jsx_text_decl_ids
            .insert(lvalue.identifier.declaration_id);
    }

    // ── Expression-level variants ──
    let expr = codegen_instruction_value(cx, &instr.value)?;

    // No lvalue → expression statement.
    // Skip pure expressions that have no side effects — upstream's codegen
    // doesn't emit them. This includes bare reads (identifiers, literals),
    // logical expressions with only pure operands (e.g., `true && null`),
    // and comparison expressions with pure operands.
    let Some(lvalue) = &instr.lvalue else {
        if is_pure_expression(&expr) {
            return None;
        }
        return Some(cx.builder.statement_expression(SPAN, expr));
    };

    let decl_id = lvalue.identifier.declaration_id;

    // Temp inlining decision (matches upstream codegenInstruction):
    // - Unnamed temporaries (name is None or Promoted) → inline into temp map
    // - Named identifiers → always emit as declaration/reassignment
    if is_temp_identifier(&lvalue.identifier) {
        if !cx.extracted_optional_deps.contains(&decl_id) {
            cx.temps.insert(decl_id, Some(expr));
        }
        return None;
    }

    let name = cx
        .name_overrides
        .get(&lvalue.identifier.declaration_id)
        .cloned()
        .unwrap_or_else(|| cx.resolve_identifier_name(&lvalue.identifier));
    let id = lvalue.identifier.id;

    // Already declared → reassignment.
    if cx.declared.contains(&id) {
        return Some(emit_assignment_stmt(cx, &name, expr));
    }

    // New declaration (expression-level always uses Const to match upstream).
    cx.declared.insert(id);
    cx.declared_decl_ids.insert(decl_id);
    Some(emit_var_decl_stmt_inner(
        cx,
        &name,
        ast::VariableDeclarationKind::Const,
        Some(expr),
        Some(decl_id),
    ))
}

// ---------------------------------------------------------------------------
// Instruction value → Expression
// ---------------------------------------------------------------------------

fn codegen_instruction_value<'a>(
    cx: &mut CodegenContext<'a>,
    value: &InstructionValue,
) -> Option<ast::Expression<'a>> {
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            codegen_place(cx, place)
        }
        InstructionValue::StoreLocal { value, .. }
        | InstructionValue::StoreContext { value, .. } => codegen_place(cx, value),
        InstructionValue::LoadGlobal { binding, .. } => Some(cx.ident_expr(binding.name())),
        InstructionValue::Primitive { value: prim, .. } => Some(lower_primitive(cx.builder, prim)),
        InstructionValue::BinaryExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_binary(
            SPAN,
            codegen_place(cx, left)?,
            lower_binary_operator(*operator),
            codegen_place(cx, right)?,
        )),
        InstructionValue::UnaryExpression {
            operator, value, ..
        } => Some(cx.builder.expression_unary(
            SPAN,
            lower_unary_operator(*operator),
            codegen_place(cx, value)?,
        )),
        InstructionValue::LogicalExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_logical(
            SPAN,
            codegen_place(cx, left)?,
            lower_logical_operator(*operator),
            codegen_place(cx, right)?,
        )),
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => Some(cx.builder.expression_conditional(
            SPAN,
            codegen_place(cx, test)?,
            codegen_place(cx, consequent)?,
            codegen_place(cx, alternate)?,
        )),
        InstructionValue::CallExpression {
            callee,
            args,
            optional,
            ..
        } => {
            let callee_expr = codegen_place(cx, callee)?;
            let is_hook = cx.options.enable_emit_hook_guards
                && !cx.options.disable_memoization_features
                && expr_is_hook_name(&callee_expr);
            let call_expr = cx.builder.expression_call(
                SPAN,
                callee_expr,
                NONE,
                codegen_arguments(cx, args)?,
                *optional,
            );
            if is_hook {
                Some(wrap_hook_guard_iife(cx, call_expr))
            } else {
                Some(call_expr)
            }
        }
        InstructionValue::NewExpression { callee, args, .. } => Some(cx.builder.expression_new(
            SPAN,
            codegen_place(cx, callee)?,
            NONE,
            codegen_arguments(cx, args)?,
        )),
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            receiver_optional,
            call_optional,
            ..
        } => {
            let callee_expr =
                codegen_method_call_callee(cx, receiver, property, *receiver_optional)?;
            // For method calls, the hook name is the property (e.g., obj.useIdentity).
            let is_hook = cx.options.enable_emit_hook_guards
                && !cx.options.disable_memoization_features
                && method_property_is_hook_name(&callee_expr);
            let call_expr = cx.builder.expression_call(
                SPAN,
                callee_expr,
                NONE,
                codegen_arguments(cx, args)?,
                *call_optional,
            );
            if is_hook {
                Some(wrap_hook_guard_iife(cx, call_expr))
            } else {
                Some(call_expr)
            }
        }
        InstructionValue::TypeCastExpression {
            value,
            type_annotation,
            type_annotation_kind,
            ..
        } => {
            let inner = codegen_place(cx, value)?;
            let ts_type = parse_ts_type(cx.allocator, &cx.builder, type_annotation);
            match type_annotation_kind {
                TypeAnnotationKind::As => Some(cx.builder.expression_ts_as(SPAN, inner, ts_type)),
                TypeAnnotationKind::Satisfies => {
                    Some(cx.builder.expression_ts_satisfies(SPAN, inner, ts_type))
                }
                TypeAnnotationKind::Cast => {
                    // Flow cast: emit `__REACT_COMPILER_FLOW_CAST__<T>(value)`.
                    // module_emitter restores this to `(value: T)` syntax.
                    let type_args = cx
                        .builder
                        .alloc_ts_type_parameter_instantiation(SPAN, cx.builder.vec1(ts_type));
                    Some(
                        cx.builder.expression_call(
                            SPAN,
                            cx.builder.expression_identifier(
                                SPAN,
                                cx.builder.ident(FLOW_CAST_MARKER_HELPER),
                            ),
                            Some(type_args),
                            cx.builder.vec1(ast::Argument::from(inner)),
                            false,
                        ),
                    )
                }
            }
        }
        InstructionValue::ArrayExpression { elements, .. } => Some(
            cx.builder
                .expression_array(SPAN, codegen_array_elements(cx, elements)?),
        ),
        InstructionValue::ObjectExpression { properties, .. } => Some(
            cx.builder
                .expression_object(SPAN, codegen_object_properties(cx, properties)?),
        ),
        InstructionValue::TemplateLiteral {
            quasis, subexprs, ..
        } => {
            let mut expressions = cx.builder.vec();
            for expr in subexprs {
                expressions.push(codegen_place(cx, expr)?);
            }
            Some(
                cx.builder.expression_template_literal(
                    SPAN,
                    cx.builder
                        .vec_from_iter(quasis.iter().enumerate().map(|(index, quasi)| {
                            cx.builder.template_element(
                                SPAN,
                                ast::TemplateElementValue {
                                    raw: cx.builder.atom(&quasi.raw),
                                    cooked: quasi.cooked.as_deref().map(|c| cx.builder.atom(c)),
                                },
                                index + 1 == quasis.len(),
                                false,
                            )
                        })),
                    expressions,
                ),
            )
        }
        InstructionValue::FunctionExpression {
            name,
            lowered_func,
            expr_type,
            ..
        } => {
            // When enableNameAnonymousFunctions is active, skip the hir_to_ast fast path
            // so that nested function expressions also get name wrapping via the reactive path.
            let result = if cx.options.enable_name_anonymous_functions
                || has_destructure_consuming_side_effecting_temp(&lowered_func.func)
            {
                None
            } else {
                super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
                    cx.builder,
                    name.as_deref(),
                    lowered_func,
                    *expr_type,
                )
            };
            let expr = if result.is_some() {
                result
            } else {
                lower_function_expression_via_reactive(
                    cx,
                    name.as_deref(),
                    lowered_func,
                    *expr_type,
                )
            };
            // Wrap anonymous functions with name hints when enableNameAnonymousFunctions is on.
            if let Some(inner_expr) = expr {
                if cx.options.enable_name_anonymous_functions
                    && name.is_none()
                    && let Some(name_hint) = lowered_func.func.id.as_deref()
                {
                    Some(wrap_named_anonymous_function_expression(
                        cx.builder, inner_expr, name_hint,
                    ))
                } else {
                    Some(inner_expr)
                }
            } else {
                None
            }
        }
        InstructionValue::JsxExpression {
            tag,
            props,
            children,
            ..
        } => {
            // SAFETY: The closure is only used synchronously within lower_jsx_expression,
            // and cx outlives the call. We use a raw pointer to satisfy the Fn + Copy bound.
            let cx_ptr = cx as *mut CodegenContext<'a>;
            let jsx_text_ids = &cx.jsx_text_decl_ids as *const HashSet<DeclarationId>;
            let fbt_ops = &cx.options.fbt_operands;
            super::super::codegen_backend::hir_to_ast::lower_jsx_expression(
                cx.builder,
                tag,
                props,
                children.as_deref(),
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                |place: &Place| unsafe {
                    (*jsx_text_ids).contains(&place.identifier.declaration_id)
                },
                fbt_ops,
                &mut HashSet::new(),
            )
        }
        InstructionValue::JsxFragment { children, .. } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            let jsx_text_ids = &cx.jsx_text_decl_ids as *const HashSet<DeclarationId>;
            super::super::codegen_backend::hir_to_ast::lower_jsx_fragment_expression(
                cx.builder,
                children,
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                |place: &Place| unsafe {
                    (*jsx_text_ids).contains(&place.identifier.declaration_id)
                },
                &mut HashSet::new(),
            )
        }
        InstructionValue::JSXText { value, .. } => Some(cx.builder.expression_string_literal(
            SPAN,
            cx.builder.atom(value),
            None,
        )),
        InstructionValue::PropertyLoad {
            object,
            property,
            optional,
            ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            super::super::codegen_backend::hir_to_ast::lower_property_load(
                cx.builder,
                object,
                property,
                *optional,
                |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                &mut HashSet::new(),
            )
        }
        InstructionValue::ComputedLoad {
            object,
            property,
            optional,
            ..
        } => Some(ast::Expression::from(
            cx.builder.member_expression_computed(
                SPAN,
                codegen_place(cx, object)?,
                codegen_place(cx, property)?,
                *optional,
            ),
        )),
        InstructionValue::PropertyStore {
            object,
            property,
            value,
            ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            let target =
                super::super::codegen_backend::hir_to_ast::lower_property_assignment_target(
                    cx.builder,
                    object,
                    property,
                    |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                    &mut HashSet::new(),
                )?;
            Some(cx.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                target,
                codegen_place(cx, value)?,
            ))
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value,
            ..
        } => Some(cx.builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(ast::SimpleAssignmentTarget::from(
                cx.builder.member_expression_computed(
                    SPAN,
                    codegen_place(cx, object)?,
                    codegen_place(cx, property)?,
                    false,
                ),
            )),
            codegen_place(cx, value)?,
        )),
        InstructionValue::PropertyDelete {
            object, property, ..
        } => {
            let cx_ptr = cx as *mut CodegenContext<'a>;
            Some(cx.builder.expression_unary(
                SPAN,
                oxc_syntax::operator::UnaryOperator::Delete,
                super::super::codegen_backend::hir_to_ast::lower_property_load(
                    cx.builder,
                    object,
                    property,
                    false,
                    |place, _visiting| unsafe { codegen_place(&mut *cx_ptr, place) },
                    &mut HashSet::new(),
                )?,
            ))
        }
        InstructionValue::ComputedDelete {
            object, property, ..
        } => Some(cx.builder.expression_unary(
            SPAN,
            oxc_syntax::operator::UnaryOperator::Delete,
            ast::Expression::from(cx.builder.member_expression_computed(
                SPAN,
                codegen_place(cx, object)?,
                codegen_place(cx, property)?,
                false,
            )),
        )),
        InstructionValue::StoreGlobal { name, value, .. } => Some(
            cx.builder.expression_assignment(
                SPAN,
                AssignmentOperator::Assign,
                ast::AssignmentTarget::from(
                    cx.builder
                        .simple_assignment_target_assignment_target_identifier(
                            SPAN,
                            cx.builder.ident(name),
                        ),
                ),
                codegen_place(cx, value)?,
            ),
        ),
        InstructionValue::PrefixUpdate {
            lvalue, operation, ..
        } => {
            let target = codegen_simple_assignment_target(cx, lvalue)?;
            Some(cx.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                true,
                target,
            ))
        }
        InstructionValue::PostfixUpdate {
            lvalue, operation, ..
        } => {
            let target = codegen_simple_assignment_target(cx, lvalue)?;
            Some(cx.builder.expression_update(
                SPAN,
                lower_update_operator(*operation),
                false,
                target,
            ))
        }
        InstructionValue::MetaProperty { meta, property, .. } => {
            Some(cx.builder.expression_meta_property(
                SPAN,
                cx.builder.identifier_name(SPAN, cx.builder.ident(meta)),
                cx.builder.identifier_name(SPAN, cx.builder.ident(property)),
            ))
        }
        InstructionValue::RegExpLiteral { pattern, flags, .. } => {
            let re_flags = parse_regexp_flags(flags);
            let regex = ast::RegExp {
                pattern: ast::RegExpPattern {
                    text: cx.builder.atom(pattern),
                    pattern: None,
                },
                flags: re_flags,
            };
            Some(cx.builder.expression_reg_exp_literal(SPAN, regex, None))
        }
        InstructionValue::TaggedTemplateExpression {
            tag, raw, cooked, ..
        } => {
            let tag_expr = codegen_place(cx, tag)?;
            let quasi = cx.builder.template_literal(
                SPAN,
                cx.builder.vec1(cx.builder.template_element(
                    SPAN,
                    ast::TemplateElementValue {
                        raw: cx.builder.atom(raw),
                        cooked: cooked.as_ref().map(|v| cx.builder.atom(v)),
                    },
                    true,
                    false,
                )),
                cx.builder.vec(),
            );
            Some(
                cx.builder
                    .expression_tagged_template(SPAN, tag_expr, NONE, quasi),
            )
        }
        InstructionValue::Await { value, .. } => {
            Some(cx.builder.expression_await(SPAN, codegen_place(cx, value)?))
        }
        InstructionValue::GetIterator { collection, .. } => codegen_place(cx, collection),
        InstructionValue::IteratorNext { collection, .. } => codegen_place(cx, collection),
        InstructionValue::NextPropertyOf { value, .. } => codegen_place(cx, value),
        InstructionValue::ObjectMethod { lowered_func, .. } => {
            let result = if has_destructure_consuming_side_effecting_temp(&lowered_func.func) {
                None
            } else {
                super::super::codegen_backend::hir_to_ast::lower_function_expression_ast(
                    cx.builder,
                    None,
                    lowered_func,
                    FunctionExpressionType::FunctionExpression,
                )
            };
            if result.is_some() {
                result
            } else {
                lower_function_expression_via_reactive(
                    cx,
                    None,
                    lowered_func,
                    FunctionExpressionType::FunctionExpression,
                )
            }
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            let mut prefix_exprs: Vec<ast::Expression<'a>> = Vec::new();
            for seq_instr in instructions {
                let is_side_effecting = is_sequence_side_effecting_value(&seq_instr.value);

                let expr = match &seq_instr.value {
                    InstructionValue::StoreLocal {
                        lvalue: store_lv,
                        value: v,
                        ..
                    }
                    | InstructionValue::StoreContext {
                        lvalue: store_lv,
                        value: v,
                        ..
                    } => {
                        let rhs = codegen_place(cx, v);
                        if matches!(store_lv.kind, InstructionKind::Reassign) {
                            rhs.map(|rhs_expr| {
                                let name = identifier_name(&store_lv.place.identifier);
                                cx.builder.expression_assignment(
                                    SPAN,
                                    AssignmentOperator::Assign,
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&name),
                                            ),
                                    ),
                                    rhs_expr,
                                )
                            })
                        } else {
                            rhs
                        }
                    }
                    _ => codegen_instruction_value(cx, &seq_instr.value),
                };
                if let Some(expr) = expr {
                    if let Some(lv) = &seq_instr.lvalue
                        && is_temp_identifier(&lv.identifier)
                    {
                        cx.temps.insert(
                            lv.identifier.declaration_id,
                            Some(expr.clone_in(cx.allocator)),
                        );
                    }
                    if is_side_effecting {
                        prefix_exprs.push(expr);
                    }
                }
            }
            // Check if the value is a LoadLocal that references the same temp
            // as a side-effecting prefix instruction.  If so, the prefix
            // expression already IS the value — we should not also inline it
            // as the final expression (which would duplicate it).
            let value_already_in_prefix = if let InstructionValue::LoadLocal {
                place: ref load_place,
                ..
            } = **value
            {
                let load_decl = load_place.identifier.declaration_id;
                // Check if any side-effecting prefix instruction's lvalue
                // matches the value's LoadLocal reference.
                instructions.iter().any(|instr| {
                    is_sequence_side_effecting_value(&instr.value)
                        && instr
                            .lvalue
                            .as_ref()
                            .is_some_and(|lv| lv.identifier.declaration_id == load_decl)
                })
            } else {
                false
            };

            if value_already_in_prefix {
                // The value expression is already present in prefix_exprs
                // from a side-effecting instruction.
                if prefix_exprs.len() == 1 {
                    Some(prefix_exprs.pop().unwrap())
                } else if prefix_exprs.is_empty() {
                    codegen_instruction_value(cx, value)
                } else {
                    Some(
                        cx.builder
                            .expression_sequence(SPAN, cx.builder.vec_from_iter(prefix_exprs)),
                    )
                }
            } else {
                let final_expr = codegen_instruction_value(cx, value)?;
                if prefix_exprs.is_empty() {
                    Some(final_expr)
                } else {
                    prefix_exprs.push(final_expr);
                    Some(
                        cx.builder
                            .expression_sequence(SPAN, cx.builder.vec_from_iter(prefix_exprs)),
                    )
                }
            }
        }
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            let expr = codegen_instruction_value(cx, value)?;
            // Wrap the optional chain result in a ChainExpression so that
            // OXC codegen emits parentheses when this expression is used as
            // the object of a non-optional member access. Without this,
            // `(props?.a).b` would be printed as `props?.a.b`, which has
            // different semantics (`.b` becomes part of the optional chain).
            Some(match expr {
                ast::Expression::StaticMemberExpression(m) => cx
                    .builder
                    .expression_chain(SPAN, ast::ChainElement::StaticMemberExpression(m)),
                ast::Expression::ComputedMemberExpression(m) => cx
                    .builder
                    .expression_chain(SPAN, ast::ChainElement::ComputedMemberExpression(m)),
                ast::Expression::CallExpression(c) => cx
                    .builder
                    .expression_chain(SPAN, ast::ChainElement::CallExpression(c)),
                other => other,
            })
        }
        InstructionValue::ReactiveLogicalExpression {
            operator,
            left,
            right,
            ..
        } => Some(cx.builder.expression_logical(
            SPAN,
            codegen_instruction_value(cx, left)?,
            lower_logical_operator(*operator),
            codegen_instruction_value(cx, right)?,
        )),
        InstructionValue::ReactiveConditionalExpression {
            test,
            consequent,
            alternate,
            ..
        } => Some(cx.builder.expression_conditional(
            SPAN,
            codegen_instruction_value(cx, test)?,
            codegen_instruction_value(cx, consequent)?,
            codegen_instruction_value(cx, alternate)?,
        )),
        // Statement-level variants handled in codegen_instruction before reaching here.
        InstructionValue::Debugger { .. }
        | InstructionValue::DeclareLocal { .. }
        | InstructionValue::DeclareContext { .. }
        | InstructionValue::Destructure { .. }
        | InstructionValue::StartMemoize { .. }
        | InstructionValue::FinishMemoize { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// Statement-level instruction helpers
// ---------------------------------------------------------------------------

fn codegen_declare<'a>(cx: &mut CodegenContext<'a>, lvalue: &LValue) -> Option<ast::Statement<'a>> {
    let name = lvalue.place.identifier.name.as_ref()?.value().to_string();
    let id = lvalue.place.identifier.id;
    let decl_id = lvalue.place.identifier.declaration_id;
    if cx.declared.contains(&id) || cx.declared_decl_ids.contains(&decl_id) {
        return None;
    }
    cx.declared.insert(id);
    cx.declared_decl_ids.insert(decl_id);
    let kind = variable_declaration_kind(lvalue.kind).unwrap_or(ast::VariableDeclarationKind::Let);
    // Check for preserved initializer from DCE (e.g., `let x = 0` where the
    // StoreLocal was rewritten to DeclareLocal but the initial value should
    // be kept because it's not always overwritten before being read).
    let init =
        crate::optimization::dead_code_elimination::preserved_top_level_let_initializer_for_decl(
            decl_id,
        )
        .map(|js_value: String| match js_value.as_str() {
            "null" => cx.builder.expression_null_literal(SPAN),
            "undefined" => cx
                .builder
                .expression_identifier(SPAN, cx.builder.ident("undefined")),
            "true" => cx.builder.expression_boolean_literal(SPAN, true),
            "false" => cx.builder.expression_boolean_literal(SPAN, false),
            s => {
                if let Ok(n) = s.parse::<f64>() {
                    cx.builder.expression_numeric_literal(
                        SPAN,
                        n,
                        None,
                        oxc_syntax::number::NumberBase::Decimal,
                    )
                } else {
                    cx.builder
                        .expression_string_literal(SPAN, cx.builder.atom(s), None)
                }
            }
        });
    Some(emit_var_decl_stmt_inner(
        cx,
        &name,
        kind,
        init,
        Some(decl_id),
    ))
}

fn codegen_store<'a>(
    cx: &mut CodegenContext<'a>,
    lvalue: &LValue,
    value: &Place,
) -> Option<ast::Statement<'a>> {
    let expr = codegen_place(cx, value)?;
    let id = lvalue.place.identifier.id;

    // Temp inlining: unnamed temporaries (Promoted by rename_variables) are
    // stored for inline substitution, matching upstream behavior.
    // Exception: Reassign kind always emits (it's modifying an existing variable).
    if is_temp_identifier(&lvalue.place.identifier)
        && !cx.declared.contains(&id)
        && !matches!(lvalue.kind, InstructionKind::Reassign)
    {
        cx.temps
            .insert(lvalue.place.identifier.declaration_id, Some(expr));
        return None;
    }

    let name = cx
        .name_overrides
        .get(&lvalue.place.identifier.declaration_id)
        .cloned()
        .unwrap_or_else(|| cx.resolve_identifier_name(&lvalue.place.identifier));

    match lvalue.kind {
        InstructionKind::Reassign => {
            // For temp-like names (tN), if the name was removed from
            // declared_names by pop_name_scope (child scope exited),
            // emit a new declaration instead of a bare assignment.
            let is_temp_like = name.starts_with('t')
                && name.len() > 1
                && name[1..].chars().all(|c| c.is_ascii_digit());
            if is_temp_like && !cx.declared_names.contains(&name) && !cx.declared.contains(&id) {
                cx.declared.insert(id);
                return Some(emit_var_decl_stmt_inner(
                    cx,
                    &name,
                    ast::VariableDeclarationKind::Let,
                    Some(expr),
                    Some(lvalue.place.identifier.declaration_id),
                ));
            }
            Some(emit_assignment_stmt(cx, &name, expr))
        }
        InstructionKind::Function | InstructionKind::HoistedFunction
            if matches!(&expr, ast::Expression::FunctionExpression(_)) =>
        {
            if let ast::Expression::FunctionExpression(func_alloc) = expr {
                let mut func = func_alloc.unbox();
                func.id = Some(cx.builder.binding_identifier(SPAN, cx.builder.ident(&name)));
                func.r#type = ast::FunctionType::FunctionDeclaration;
                cx.declared.insert(id);
                Some(ast::Statement::FunctionDeclaration(cx.builder.alloc(func)))
            } else {
                unreachable!()
            }
        }
        kind => {
            if cx.declared.contains(&id) {
                Some(emit_assignment_stmt(cx, &name, expr))
            } else {
                cx.declared.insert(id);
                let decl_kind = match kind {
                    InstructionKind::Const | InstructionKind::HoistedConst => {
                        ast::VariableDeclarationKind::Const
                    }
                    InstructionKind::Function | InstructionKind::HoistedFunction => {
                        ast::VariableDeclarationKind::Const
                    }
                    _ => ast::VariableDeclarationKind::Let,
                };
                Some(emit_var_decl_stmt_inner(
                    cx,
                    &name,
                    decl_kind,
                    Some(expr),
                    Some(lvalue.place.identifier.declaration_id),
                ))
            }
        }
    }
}

fn codegen_destructure<'a>(
    cx: &mut CodegenContext<'a>,
    lvalue: &LValuePattern,
    value: &Place,
) -> Option<ast::Statement<'a>> {
    let rhs = codegen_place(cx, value)?;
    let mut kind = variable_declaration_kind(lvalue.kind);
    // If all pattern variables are already declared (by scope prologue),
    // treat as reassignment even if the original kind was Let/Const.
    if kind.is_some() && all_pattern_vars_declared(cx, &lvalue.pattern) {
        kind = None;
    }
    if let Some(kind) = kind {
        // Declaration: const [a, b] = expr;
        let pattern = build_binding_pattern_from_pattern(cx, &lvalue.pattern)?;
        Some(ast::Statement::VariableDeclaration(
            cx.builder.alloc_variable_declaration(
                SPAN,
                kind,
                cx.builder.vec1(cx.builder.variable_declarator(
                    SPAN,
                    kind,
                    pattern,
                    NONE,
                    Some(rhs),
                    false,
                )),
                false,
            ),
        ))
    } else {
        // Reassignment: [a, b] = expr;
        let target = build_assignment_target_from_pattern(cx, &lvalue.pattern)?;
        Some(
            cx.builder.statement_expression(
                SPAN,
                cx.builder
                    .expression_assignment(SPAN, AssignmentOperator::Assign, target, rhs),
            ),
        )
    }
}

fn build_binding_pattern_from_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    pattern: &Pattern,
) -> Option<ast::BindingPattern<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = cx.builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        elements.push(Some(
                            cx.builder
                                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&name)),
                        ));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        rest =
                            Some(cx.builder.alloc_binding_rest_element(
                                SPAN,
                                cx.builder.binding_pattern_binding_identifier(
                                    SPAN,
                                    cx.builder.ident(&name),
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
            Some(
                cx.builder
                    .binding_pattern_array_pattern(SPAN, elements, rest),
            )
        }
        Pattern::Object(obj) => {
            let mut properties = cx.builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if rest.is_some() {
                            return None;
                        }
                        let target_name = identifier_name(&property.place.identifier);
                        cx.declared.insert(property.place.identifier.id);
                        let (key, computed) = build_property_key_for_pattern(cx, &property.key)?;
                        let shorthand = matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        );
                        properties.push(cx.builder.binding_property(
                            SPAN,
                            key,
                            cx.builder.binding_pattern_binding_identifier(
                                SPAN,
                                cx.builder.ident(&target_name),
                            ),
                            shorthand,
                            computed,
                        ));
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        if rest.is_some() || index + 1 != obj.properties.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        cx.declared.insert(place.identifier.id);
                        rest =
                            Some(cx.builder.alloc_binding_rest_element(
                                SPAN,
                                cx.builder.binding_pattern_binding_identifier(
                                    SPAN,
                                    cx.builder.ident(&name),
                                ),
                            ));
                    }
                }
            }
            Some(
                cx.builder
                    .binding_pattern_object_pattern(SPAN, properties, rest),
            )
        }
    }
}

fn build_assignment_target_from_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    pattern: &Pattern,
) -> Option<ast::AssignmentTarget<'a>> {
    match pattern {
        Pattern::Array(arr) => {
            let mut elements = cx.builder.vec();
            let mut rest = None;
            for (index, item) in arr.items.iter().enumerate() {
                match item {
                    ArrayElement::Place(place) => {
                        if rest.is_some() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        elements.push(Some(ast::AssignmentTargetMaybeDefault::from(
                            ast::AssignmentTarget::from(
                                cx.builder
                                    .simple_assignment_target_assignment_target_identifier(
                                        SPAN,
                                        cx.builder.ident(&name),
                                    ),
                            ),
                        )));
                    }
                    ArrayElement::Spread(place) => {
                        if rest.is_some() || index + 1 != arr.items.len() {
                            return None;
                        }
                        let name = identifier_name(&place.identifier);
                        rest = Some(
                            cx.builder.alloc_assignment_target_rest(
                                SPAN,
                                ast::AssignmentTarget::from(
                                    cx.builder
                                        .simple_assignment_target_assignment_target_identifier(
                                            SPAN,
                                            cx.builder.ident(&name),
                                        ),
                                ),
                            ),
                        );
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
                cx.builder
                    .assignment_target_pattern_array_assignment_target(SPAN, elements, rest),
            ))
        }
        Pattern::Object(obj) => {
            let mut properties = cx.builder.vec();
            let mut rest = None;
            for (index, prop) in obj.properties.iter().enumerate() {
                match prop {
                    ObjectPropertyOrSpread::Property(property) => {
                        if rest.is_some() {
                            return None;
                        }
                        let target_name = identifier_name(&property.place.identifier);
                        if matches!(
                            &property.key,
                            ObjectPropertyKey::Identifier(name) if name == &target_name
                        ) {
                            properties.push(
                                cx.builder
                                    .assignment_target_property_assignment_target_property_identifier(
                                        SPAN,
                                        cx.builder.identifier_reference(
                                            SPAN,
                                            cx.builder.ident(&target_name),
                                        ),
                                        None,
                                    ),
                            );
                            continue;
                        }
                        let (key, computed) = build_property_key_for_pattern(cx, &property.key)?;
                        properties.push(
                            cx.builder
                                .assignment_target_property_assignment_target_property_property(
                                SPAN,
                                key,
                                ast::AssignmentTargetMaybeDefault::from(
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&target_name),
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
                        let name = identifier_name(&place.identifier);
                        rest = Some(
                            cx.builder.alloc_assignment_target_rest(
                                SPAN,
                                ast::AssignmentTarget::from(
                                    cx.builder
                                        .simple_assignment_target_assignment_target_identifier(
                                            SPAN,
                                            cx.builder.ident(&name),
                                        ),
                                ),
                            ),
                        );
                    }
                }
            }
            Some(ast::AssignmentTarget::from(
                cx.builder
                    .assignment_target_pattern_object_assignment_target(SPAN, properties, rest),
            ))
        }
    }
}

fn build_property_key_for_pattern<'a>(
    cx: &mut CodegenContext<'a>,
    key: &ObjectPropertyKey,
) -> Option<(ast::PropertyKey<'a>, bool)> {
    match key {
        ObjectPropertyKey::Identifier(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
        )),
        ObjectPropertyKey::String(name) => Some((
            ast::PropertyKey::from(cx.builder.expression_string_literal(
                SPAN,
                cx.builder.atom(name),
                None,
            )),
            false,
        )),
        ObjectPropertyKey::Number(val) => Some((
            ast::PropertyKey::from(cx.builder.expression_numeric_literal(
                SPAN,
                *val,
                None,
                NumberBase::Decimal,
            )),
            false,
        )),
        ObjectPropertyKey::Computed(place) => {
            Some((ast::PropertyKey::from(codegen_place(cx, place)?), true))
        }
    }
}

// ---------------------------------------------------------------------------
// Emit helpers
// ---------------------------------------------------------------------------

/// Check if an identifier is a temporary (truly unnamed).
/// Promoted `tN` names are runtime bindings that earlier passes chose to
/// materialize, so re-inlining them breaks evaluation order. Only truly
/// unnamed identifiers are eligible for temp inlining (matches string
/// codegen's `is_temp_like_identifier`).
fn is_temp_identifier(identifier: &Identifier) -> bool {
    identifier.name.is_none()
}

/// Check if an instruction value has side effects that must be preserved in a
/// sequence expression.  Pure intermediate computations (loads, property reads,
/// primitives) should be inlined via the temp map, while side-effecting
/// Check if an AST expression is pure (no side effects). Pure expressions
/// include identifiers, literals, and logical/comparison expressions with
/// only pure operands.
fn is_pure_expression(expr: &ast::Expression) -> bool {
    match expr {
        ast::Expression::Identifier(_)
        | ast::Expression::NullLiteral(_)
        | ast::Expression::BooleanLiteral(_)
        | ast::Expression::NumericLiteral(_)
        | ast::Expression::StringLiteral(_) => true,
        ast::Expression::LogicalExpression(logical) => {
            is_pure_expression(&logical.left) && is_pure_expression(&logical.right)
        }
        ast::Expression::BinaryExpression(binary) => {
            is_pure_expression(&binary.left) && is_pure_expression(&binary.right)
        }
        ast::Expression::UnaryExpression(unary) => {
            // typeof, void, !, ~, +, - are pure; delete is not
            !matches!(unary.operator, oxc_syntax::operator::UnaryOperator::Delete)
                && is_pure_expression(&unary.argument)
        }
        _ => false,
    }
}

/// operations (calls, assignments, updates) must appear in the comma-separated
/// expression list.
fn is_sequence_side_effecting_value(value: &InstructionValue) -> bool {
    matches!(
        value,
        InstructionValue::CallExpression { .. }
            | InstructionValue::MethodCall { .. }
            | InstructionValue::PostfixUpdate { .. }
            | InstructionValue::PrefixUpdate { .. }
            | InstructionValue::StoreLocal {
                lvalue: crate::hir::types::LValue {
                    kind: InstructionKind::Reassign,
                    ..
                },
                ..
            }
            | InstructionValue::StoreContext {
                lvalue: crate::hir::types::LValue {
                    kind: InstructionKind::Reassign,
                    ..
                },
                ..
            }
            | InstructionValue::ReactiveSequenceExpression { .. }
    )
}

fn identifier_name(identifier: &Identifier) -> String {
    identifier
        .name
        .as_ref()
        .map(|n| n.value().to_string())
        .unwrap_or_else(|| {
            // Record invariant error matching upstream convertIdentifier()
            CODEGEN_UNNAMED_ERROR.with(|slot| {
                let mut slot = slot.borrow_mut();
                if slot.is_none() {
                    *slot = Some(CompilerError::invariant(
                        "Expected temporaries to be promoted to named identifiers in an earlier pass",
                        Some(format!("identifier {} is unnamed", identifier.id.0)),
                    ));
                }
            });
            format!("t{}", identifier.id.0)
        })
}

fn variable_declaration_kind(kind: InstructionKind) -> Option<ast::VariableDeclarationKind> {
    match kind {
        InstructionKind::Const | InstructionKind::HoistedConst => {
            Some(ast::VariableDeclarationKind::Const)
        }
        InstructionKind::Let | InstructionKind::Catch | InstructionKind::HoistedLet => {
            Some(ast::VariableDeclarationKind::Let)
        }
        InstructionKind::Reassign
        | InstructionKind::Function
        | InstructionKind::HoistedFunction => None,
    }
}

/// Wrap JSX elements/fragments in parentheses to match Babel's printer behavior.
fn maybe_parenthesize_jsx<'a>(
    builder: AstBuilder<'a>,
    expr: ast::Expression<'a>,
) -> ast::Expression<'a> {
    if matches!(
        &expr,
        ast::Expression::JSXElement(_) | ast::Expression::JSXFragment(_)
    ) {
        builder.expression_parenthesized(SPAN, expr)
    } else {
        expr
    }
}

fn emit_assignment_stmt<'a>(
    cx: &mut CodegenContext<'a>,
    name: &str,
    expr: ast::Expression<'a>,
) -> ast::Statement<'a> {
    let expr = maybe_parenthesize_jsx(cx.builder, expr);
    cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_assignment(
            SPAN,
            AssignmentOperator::Assign,
            ast::AssignmentTarget::from(
                cx.builder
                    .simple_assignment_target_assignment_target_identifier(
                        SPAN,
                        cx.builder.ident(name),
                    ),
            ),
            expr,
        ),
    )
}

#[allow(dead_code)]
fn emit_var_decl_stmt<'a>(
    cx: &mut CodegenContext<'a>,
    name: &str,
    kind: ast::VariableDeclarationKind,
    init: Option<ast::Expression<'a>>,
) -> ast::Statement<'a> {
    emit_var_decl_stmt_inner(cx, name, kind, init, None)
}

fn emit_var_decl_stmt_inner<'a>(
    cx: &mut CodegenContext<'a>,
    name: &str,
    kind: ast::VariableDeclarationKind,
    init: Option<ast::Expression<'a>>,
    decl_id: Option<DeclarationId>,
) -> ast::Statement<'a> {
    // Prevent duplicate `let`/`const` for the same logical variable.
    // When a DIFFERENT DeclarationId uses the same name (block-scoped shadowing),
    // allow the new declaration — this is valid JS.
    if cx.declared_names.contains(name) {
        let is_same_variable = decl_id.is_some_and(|did| cx.declared_decl_ids.contains(&did));
        if is_same_variable {
            if let Some(expr) = init {
                return emit_assignment_stmt(cx, name, expr);
            }
            return cx.builder.statement_empty(SPAN);
        }
        // Different DeclarationId — allow new declaration (block-scoped shadowing).
    }
    cx.declared_names.insert(name.to_string());
    cx.register_scoped_name(name);
    if let Some(did) = decl_id {
        cx.declared_decl_ids.insert(did);
    }
    let init = init.map(|e| maybe_parenthesize_jsx(cx.builder, e));
    let pattern = cx
        .builder
        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name));
    ast::Statement::VariableDeclaration(
        cx.builder.alloc_variable_declaration(
            SPAN,
            kind,
            cx.builder.vec1(
                cx.builder
                    .variable_declarator(SPAN, kind, pattern, NONE, init, false),
            ),
            false,
        ),
    )
}

fn parse_regexp_flags(flags: &str) -> ast::RegExpFlags {
    let mut result = ast::RegExpFlags::empty();
    for c in flags.chars() {
        result |= match c {
            'g' => ast::RegExpFlags::G,
            'i' => ast::RegExpFlags::I,
            'm' => ast::RegExpFlags::M,
            's' => ast::RegExpFlags::S,
            'u' => ast::RegExpFlags::U,
            'y' => ast::RegExpFlags::Y,
            'd' => ast::RegExpFlags::D,
            'v' => ast::RegExpFlags::V,
            _ => ast::RegExpFlags::empty(),
        };
    }
    result
}

// ---------------------------------------------------------------------------
// Place → Expression
// ---------------------------------------------------------------------------

fn codegen_place<'a>(cx: &mut CodegenContext<'a>, place: &Place) -> Option<ast::Expression<'a>> {
    let decl_id = place.identifier.declaration_id;

    // Only check temp map for unnamed/promoted identifiers (temporaries).
    // Named identifiers always emit as identifier references, even if
    // something was stored in the temp map for their declaration_id.
    // Also check force_inline_decls for named identifiers that were suppressed
    // by the destructure+call fusion pre-scan.
    if (is_temp_identifier(&place.identifier) || cx.force_inline_decls.contains(&decl_id))
        && let Some(temp_slot) = cx.temps.get_mut(&decl_id)
        && let Some(expr) = temp_slot.as_ref()
    {
        return Some(expr.clone_in(cx.allocator));
    }

    // Check name override for this declaration (from string codegen).
    if let Some(override_name) = cx.name_overrides.get(&decl_id) {
        return Some(cx.ident_expr(override_name));
    }

    // Check decl_names for shifted/overridden names (e.g. scope dep/output collision).
    if let Some(name) = cx.decl_names.get(&decl_id) {
        return Some(cx.ident_expr(name));
    }

    // Use identifier name.
    if let Some(name) = place.identifier.name.as_ref() {
        let s = name.value().to_string();
        cx.decl_names.insert(decl_id, s.clone());
        return Some(cx.ident_expr(&s));
    }

    // Last resort: use temp name based on DeclarationId.
    Some(cx.ident_expr(&format!("t{}", decl_id.0)))
}

fn codegen_simple_assignment_target<'a>(
    cx: &mut CodegenContext<'a>,
    place: &Place,
) -> Option<ast::SimpleAssignmentTarget<'a>> {
    let expr = codegen_place(cx, place)?;
    super::super::codegen_backend::hir_to_ast::expression_to_simple_assignment_target(
        cx.builder, expr,
    )
}

// ---------------------------------------------------------------------------
// Terminal → Statement(s)
// ---------------------------------------------------------------------------

fn codegen_terminal<'a>(
    cx: &mut CodegenContext<'a>,
    terminal: &ReactiveTerminal,
) -> Vec<ast::Statement<'a>> {
    match terminal {
        ReactiveTerminal::Return { value, .. } => {
            let expr = codegen_place(cx, value);
            // Use implicit undefined: emit `return;` instead of `return undefined;`
            // to match upstream CodegenReactiveFunction.ts behavior.
            let expr = expr.filter(
                |e| !matches!(e, ast::Expression::Identifier(id) if id.name == "undefined"),
            );
            vec![cx.builder.statement_return(SPAN, expr)]
        }
        ReactiveTerminal::Throw { value, .. } => {
            let Some(expr) = codegen_place(cx, value) else {
                return vec![];
            };
            vec![cx.builder.statement_throw(SPAN, expr)]
        }
        ReactiveTerminal::Break {
            target,
            target_kind,
            ..
        } => {
            if *target_kind == ReactiveTerminalTargetKind::Implicit {
                return vec![];
            }
            let label = if *target_kind == ReactiveTerminalTargetKind::Labeled {
                Some(cx.builder.label_identifier(SPAN, crate_label_name(*target)))
            } else {
                None
            };
            vec![cx.builder.statement_break(SPAN, label)]
        }
        ReactiveTerminal::Continue {
            target,
            target_kind,
            ..
        } => {
            if *target_kind == ReactiveTerminalTargetKind::Implicit {
                return vec![];
            }
            let label = if *target_kind == ReactiveTerminalTargetKind::Labeled {
                Some(cx.builder.label_identifier(SPAN, crate_label_name(*target)))
            } else {
                None
            };
            vec![cx.builder.statement_continue(SPAN, label)]
        }
        ReactiveTerminal::If {
            test,
            consequent,
            alternate,
            ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            cx.push_name_scope();
            let consequent_stmts = codegen_block(cx, consequent);
            cx.pop_name_scope();
            let alternate_result = alternate.as_ref().map(|alt| {
                cx.push_name_scope();
                let stmts = codegen_block(cx, alt);
                cx.pop_name_scope();
                stmts
            });
            // When both branches are empty, emit as empty if-statement (not bare
            // expression) to match upstream behavior. Upstream keeps `if(a){}`.
            if consequent_stmts.is_empty()
                && alternate_result.as_ref().is_some_and(|a| a.is_empty())
            {
                return vec![cx.builder.statement_if(
                    SPAN,
                    test_expr,
                    cx.builder.statement_block(SPAN, cx.builder.vec()),
                    None,
                )];
            }
            let consequent_block = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(consequent_stmts));
            let alternate_stmt = alternate_result.and_then(|alt_stmts| {
                if alt_stmts.is_empty() {
                    None // Skip empty else clauses
                } else {
                    Some(
                        cx.builder
                            .statement_block(SPAN, cx.builder.vec_from_iter(alt_stmts)),
                    )
                }
            });
            vec![
                cx.builder
                    .statement_if(SPAN, test_expr, consequent_block, alternate_stmt),
            ]
        }
        ReactiveTerminal::Switch { test, cases, .. } => {
            let Some(discriminant) = codegen_place(cx, test) else {
                return vec![];
            };
            let mut switch_cases = cx.builder.vec();
            for case in cases {
                let test_expr = case.test.as_ref().and_then(|t| codegen_place(cx, t));
                let mut consequent = case
                    .block
                    .as_ref()
                    .map(|b| codegen_block(cx, b))
                    .unwrap_or_default();
                // If the case body is only a `break;` with no other content,
                // treat it as fallthrough (suppress the break).
                // If the case body is only a bare `break;`, treat as fallthrough.
                if consequent.len() == 1
                    && matches!(&consequent[0], ast::Statement::BreakStatement(b) if b.label.is_none())
                {
                    consequent.clear();
                }
                // Wrap case body in a block for Babel-compatible output.
                let wrapped = if consequent.is_empty() {
                    cx.builder.vec()
                } else {
                    cx.builder.vec1(
                        cx.builder
                            .statement_block(SPAN, cx.builder.vec_from_iter(consequent)),
                    )
                };
                switch_cases.push(cx.builder.switch_case(SPAN, test_expr, wrapped));
            }
            vec![
                cx.builder
                    .statement_switch(SPAN, discriminant, switch_cases),
            ]
        }
        ReactiveTerminal::While {
            test, loop_block, ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            cx.push_name_scope();
            let body_stmts = codegen_block(cx, loop_block);
            cx.pop_name_scope();
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_while(SPAN, test_expr, body)]
        }
        ReactiveTerminal::DoWhile {
            test, loop_block, ..
        } => {
            let Some(test_expr) = codegen_place(cx, test) else {
                return vec![];
            };
            cx.push_name_scope();
            let body_stmts = codegen_block(cx, loop_block);
            cx.pop_name_scope();
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_do_while(SPAN, body, test_expr)]
        }
        ReactiveTerminal::For {
            init,
            test,
            update,
            update_value,
            loop_block,
            ..
        } => {
            // Init: emit instructions from init block, reconstructing merged
            // declarations.  The HIR may split `let i = expr;` into separate
            // DeclareLocal + StoreLocal instructions, producing `let i;` then
            // `i = expr;`.  We merge these back into a single VariableDeclaration
            // with the initializer, matching upstream Babel output.
            let init_stmts = codegen_block(cx, init);
            let for_init = reconstruct_for_init(cx, init_stmts);

            let test_expr = codegen_place(cx, test);

            // Update expression.
            //
            // Strategy: process the update block's instructions to populate
            // the temp map, then find the StoreLocal/Reassign instruction
            // within the block to construct the assignment expression.  The
            // `update_value` may be a trailing `LoadLocal(x)` that merely
            // reads back the result; the actual side-effect we need is the
            // `StoreLocal { x = expr }` inside the block.
            let update_expr = if let Some(update_block) = update {
                // Process all instructions into the temp map / statement list.
                let update_stmts = codegen_block(cx, update_block);

                // Prefer: find the assignment/update expression produced by the
                // StoreLocal/Reassign in the emitted statements.
                let assign_expr = update_stmts.iter().rev().find_map(|s| {
                    if let ast::Statement::ExpressionStatement(es) = s {
                        if matches!(
                            &es.expression,
                            ast::Expression::AssignmentExpression(_)
                                | ast::Expression::UpdateExpression(_)
                        ) {
                            Some(es.expression.clone_in(cx.allocator))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                });

                if let Some(assign) = assign_expr {
                    // If there's also a trailing value expression (LoadLocal),
                    // combine them into a sequence: `(assign, value)`
                    if let Some(uv) = update_value {
                        if let InstructionValue::LoadLocal { place, .. }
                        | InstructionValue::LoadContext { place, .. } = uv.as_ref()
                        {
                            if let Some(val) = codegen_place(cx, place) {
                                Some(cx.builder.expression_sequence(
                                    SPAN,
                                    cx.builder.vec_from_array([assign, val]),
                                ))
                            } else {
                                Some(assign)
                            }
                        } else {
                            Some(assign)
                        }
                    } else {
                        Some(assign)
                    }
                } else if let Some(uv) = update_value {
                    // Fallback to codegen_instruction_value on the
                    // update_value when no assignment was found.
                    match uv.as_ref() {
                        InstructionValue::StoreLocal { lvalue, value, .. }
                        | InstructionValue::StoreContext { lvalue, value, .. }
                            if matches!(lvalue.kind, InstructionKind::Reassign) =>
                        {
                            if let Some(rhs) = codegen_place(cx, value) {
                                let name = identifier_name(&lvalue.place.identifier);
                                Some(cx.builder.expression_assignment(
                                    SPAN,
                                    AssignmentOperator::Assign,
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&name),
                                            ),
                                    ),
                                    rhs,
                                ))
                            } else {
                                None
                            }
                        }
                        // Read-only update values (LoadLocal, LoadContext) indicate
                        // a no-op update — don't emit them as for-loop update expr.
                        InstructionValue::LoadLocal { .. }
                        | InstructionValue::LoadContext { .. } => None,
                        _ => codegen_instruction_value(cx, uv),
                    }
                } else {
                    // Extract expression from last statement.
                    update_stmts.into_iter().last().and_then(|s| {
                        if let ast::Statement::ExpressionStatement(es) = s {
                            Some(es.unbox().expression)
                        } else {
                            None
                        }
                    })
                }
            } else if let Some(uv) = update_value {
                codegen_instruction_value(cx, uv)
            } else {
                None
            };

            cx.push_name_scope();
            let body_stmts = codegen_block(cx, loop_block);
            cx.pop_name_scope();
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![
                cx.builder
                    .statement_for(SPAN, for_init, test_expr, update_expr, body),
            ]
        }
        ReactiveTerminal::ForOf {
            init, loop_block, ..
        } => {
            // Extract collection from IteratorNext/NextPropertyOf in init,
            // and resolve it BEFORE processing the init block (which may
            // consume the collection's temp).
            let collection_place = extract_for_collection(init);
            let right = if let Some(cp) = &collection_place {
                codegen_place(cx, cp)
            } else {
                None
            };
            let init_stmts = codegen_block(cx, init);
            let left = extract_for_of_left(cx, init_stmts);
            let (Some(left), Some(right)) = (left, right) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_for_of(SPAN, false, left, right, body)]
        }
        ReactiveTerminal::ForIn {
            init, loop_block, ..
        } => {
            let collection_place = extract_for_collection(init);
            let right = if let Some(cp) = &collection_place {
                codegen_place(cx, cp)
            } else {
                None
            };
            let init_stmts = codegen_block(cx, init);
            let left = extract_for_of_left(cx, init_stmts);
            let (Some(left), Some(right)) = (left, right) else {
                return vec![];
            };
            let body_stmts = codegen_block(cx, loop_block);
            let body = cx
                .builder
                .statement_block(SPAN, cx.builder.vec_from_iter(body_stmts));
            vec![cx.builder.statement_for_in(SPAN, left, right, body)]
        }
        ReactiveTerminal::Label { block, .. } => codegen_block(cx, block),
        ReactiveTerminal::Try {
            block,
            handler_binding,
            handler,
            ..
        } => {
            let try_stmts = codegen_block(cx, block);
            let try_block = cx
                .builder
                .block_statement(SPAN, cx.builder.vec_from_iter(try_stmts));

            let catch_param = handler_binding.as_ref().and_then(|binding| {
                let name = binding.identifier.name.as_ref()?.value();
                Some(
                    cx.builder.catch_parameter(
                        SPAN,
                        cx.builder
                            .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name)),
                        NONE,
                    ),
                )
            });

            let catch_stmts = codegen_block(cx, handler);
            let catch_body = cx
                .builder
                .block_statement(SPAN, cx.builder.vec_from_iter(catch_stmts));
            let catch_clause = cx.builder.alloc_catch_clause(SPAN, catch_param, catch_body);

            vec![cx.builder.statement_try(
                SPAN,
                try_block,
                Some(catch_clause),
                Option::<oxc_allocator::Box<'_, ast::BlockStatement<'_>>>::None,
            )]
        }
    }
}

// ---------------------------------------------------------------------------
// Optional chain dependency extraction helpers
// ---------------------------------------------------------------------------

/// Check if an AST expression structurally matches a dependency path.
/// Walks the member expression chain backward and compares each step with the
/// dep path entries (property name and optional flag).
fn expr_matches_dep_path(expr: &ast::Expression, dep: &ReactiveScopeDependency) -> bool {
    expr_matches_dep_path_inner(expr, dep, false).0
}

/// Like `expr_matches_dep_path` but also allows the expression to have
/// `optional: true` where the dep path has `optional: false`. This handles
/// the case where `DeriveMinimalDependenciesHIR` converted an optional access
/// to unconditional because the hoistable tree said the base was NonNull.
///
/// Returns `(matches, has_optional_mismatch)`. When `has_optional_mismatch`
/// is true, the expression contains optional chains that were stripped from
/// the dependency path and the temp should be extracted.
fn expr_matches_dep_path_relaxed(
    expr: &ast::Expression,
    dep: &ReactiveScopeDependency,
) -> (bool, bool) {
    expr_matches_dep_path_inner(expr, dep, true)
}

/// Inner matching function.
/// When `allow_optional_mismatch` is true, an expression member with
/// `optional: true` is allowed to match a dep entry with `optional: false`.
/// Returns `(matches, has_optional_mismatch)`.
fn expr_matches_dep_path_inner(
    expr: &ast::Expression,
    dep: &ReactiveScopeDependency,
    allow_optional_mismatch: bool,
) -> (bool, bool) {
    let mut current = expr;
    let mut path_idx = dep.path.len();
    let mut has_optional_mismatch = false;

    while path_idx > 0 {
        path_idx -= 1;
        let entry = &dep.path[path_idx];
        match current {
            ast::Expression::StaticMemberExpression(member) => {
                if member.property.name.as_str() != entry.property {
                    return (false, false);
                }
                if member.optional != entry.optional {
                    if allow_optional_mismatch && member.optional && !entry.optional {
                        has_optional_mismatch = true;
                    } else {
                        return (false, false);
                    }
                }
                current = &member.object;
            }
            ast::Expression::ComputedMemberExpression(member) => {
                if let ast::Expression::NumericLiteral(lit) = &member.expression {
                    if lit.value.to_string() != entry.property {
                        return (false, false);
                    }
                } else {
                    return (false, false);
                }
                if member.optional != entry.optional {
                    if allow_optional_mismatch && member.optional && !entry.optional {
                        has_optional_mismatch = true;
                    } else {
                        return (false, false);
                    }
                }
                current = &member.object;
            }
            _ => return (false, false),
        }
    }

    // Check root identifier.
    if let ast::Expression::Identifier(ident) = current
        && let Some(name) = dep.identifier.name.as_ref()
    {
        return (ident.name.as_str() == name.value(), has_optional_mismatch);
    }
    (false, false)
}

/// Search `cx.temps` for a DeclarationId whose stored expression matches
/// the given optional-chain dependency path.
fn find_temp_matching_dep(
    cx: &CodegenContext,
    dep: &ReactiveScopeDependency,
) -> Option<DeclarationId> {
    for (&decl_id, expr_opt) in &cx.temps {
        if let Some(expr) = expr_opt
            && expr_matches_dep_path(expr, dep)
        {
            return Some(decl_id);
        }
    }
    None
}

/// Search `cx.temps` for a DeclarationId whose stored expression matches
/// the dep path but has optional chains that were stripped from the dep path
/// by the hoistable-tree conversion.
///
/// Only returns a result if there is NO exact (unconditional) temp match for
/// the same dep path. This avoids incorrectly extracting an optional temp
/// when an unconditional temp already covers the dependency.
fn find_temp_with_optional_chain_for_dep(
    cx: &CodegenContext,
    dep: &ReactiveScopeDependency,
) -> Option<DeclarationId> {
    // First check: is there already an exact unconditional match?
    // If so, no extraction needed -- the dep can be codegen'd directly.
    for expr_opt in cx.temps.values() {
        if let Some(expr) = expr_opt
            && expr_matches_dep_path(expr, dep)
        {
            return None;
        }
    }
    // No exact match. Try relaxed matching to find a temp with optional
    // chains that correspond to this dependency.
    for (&decl_id, expr_opt) in &cx.temps {
        if let Some(expr) = expr_opt {
            let (matches, has_optional) = expr_matches_dep_path_relaxed(expr, dep);
            if matches && has_optional {
                return Some(decl_id);
            }
        }
    }
    None
}

/// Check if the scope body contains ANY PropertyLoad or ComputedLoad instructions
/// at any nesting level. If so, the body likely computes its own property chains
/// and the extraction of external temps would be incorrect.
fn body_has_any_property_loads(instructions: &ReactiveBlock) -> bool {
    for stmt in instructions {
        if let ReactiveStatement::Instruction(instr) = stmt
            && value_has_any_property_load(&instr.value)
        {
            return true;
        }
    }
    false
}

fn value_has_any_property_load(value: &InstructionValue) -> bool {
    match value {
        InstructionValue::PropertyLoad { .. } | InstructionValue::ComputedLoad { .. } => true,
        InstructionValue::ReactiveOptionalExpression { value, .. } => {
            value_has_any_property_load(value)
        }
        InstructionValue::ReactiveSequenceExpression {
            instructions,
            value,
            ..
        } => {
            instructions
                .iter()
                .any(|i| value_has_any_property_load(&i.value))
                || value_has_any_property_load(value)
        }
        _ => false,
    }
}

/// Allocate a fresh `tN` temp name that doesn't conflict with existing declarations.
fn alloc_fresh_temp_name(cx: &CodegenContext) -> String {
    let mut idx = 0u32;
    loop {
        let candidate = format!("t{idx}");
        if !cx.declared_names.contains(&candidate)
            && !cx.options.unique_identifiers.contains(&candidate)
        {
            return candidate;
        }
        idx += 1;
    }
}

// ---------------------------------------------------------------------------
// Reactive scope (memoization)
// ---------------------------------------------------------------------------

fn codegen_reactive_scope<'a>(
    cx: &mut CodegenContext<'a>,
    scope: &ReactiveScope,
    instructions: &ReactiveBlock,
) -> Vec<ast::Statement<'a>> {
    // Skip scopes with no declarations and no reassignments — they produce
    // no memoized output and should not allocate cache slots.
    if scope.declarations.is_empty() && scope.reassignments.is_empty() {
        return codegen_block(cx, instructions);
    }

    // Inline zero-dep scopes whose declarations are all truly unnamed.
    if scope.dependencies.is_empty()
        && scope.reassignments.is_empty()
        && scope
            .declarations
            .values()
            .all(|d| d.identifier.name.is_none())
    {
        return codegen_block(cx, instructions);
    }

    // Inline zero-dep single-declaration scopes with strictly trivial body
    // (no calls, no JSX, no allocations). AND primitive-typed scopes with
    // trivial+calls body. Both are safe to inline since they can't produce
    // objects/arrays that need referential stability.
    if scope.dependencies.is_empty()
        && scope.reassignments.is_empty()
        && scope.declarations.len() == 1
    {
        let decl = scope.declarations.values().next().unwrap();
        let is_prim = matches!(decl.identifier.type_, Type::Primitive);
        if is_prim && scope_body_is_trivial_or_calls(instructions) {
            return codegen_block(cx, instructions);
        }
        if scope_body_is_strictly_trivial(instructions) {
            return codegen_block(cx, instructions);
        }
    }

    let mut stmts = Vec::new();

    // Collect catch binding declaration_ids from Try terminals in this scope.
    // These must NOT be emitted as `let` declarations — they are implicitly
    // declared by the `catch (e)` clause.
    let catch_binding_decl_ids: HashSet<DeclarationId> = instructions
        .iter()
        .filter_map(|stmt| match stmt {
            ReactiveStatement::Terminal(term) => {
                if let ReactiveTerminal::Try {
                    handler_binding, ..
                } = &term.terminal
                {
                    handler_binding
                        .as_ref()
                        .map(|b| b.identifier.declaration_id)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();

    // Emit declarations for scope-declared variables (before the memoization guard).
    // Sort by name for deterministic output matching upstream.
    let mut decl_names: Vec<(String, IdentifierId, DeclarationId)> = scope
        .declarations
        .iter()
        .filter_map(|(id, decl)| {
            // Skip catch binding declarations — they're declared by the catch clause.
            if catch_binding_decl_ids.contains(&decl.identifier.declaration_id) {
                return None;
            }
            let name = decl.identifier.name.as_ref()?.value().to_string();
            Some((name, *id, decl.identifier.declaration_id))
        })
        .collect();
    decl_names.sort_by(|a, b| a.0.cmp(&b.0));

    // Collect dependency root names to detect dep/output name collisions.
    let dep_root_names: HashSet<String> = scope
        .dependencies
        .iter()
        .filter_map(|d| Some(d.identifier.name.as_ref()?.value().to_string()))
        .collect();

    // Pre-compute shifted output names for scope declarations whose temp name
    // collides with a dependency name. This avoids circular references like
    // `if ($[0] !== t0) { t0 = ...; $[1] = t0 }` by shifting the output to t1.
    let mut shifted_names: HashMap<DeclarationId, String> = HashMap::new();
    for (name, _, decl_id) in &decl_names {
        let is_temp = name.starts_with('t')
            && name.len() >= 2
            && name[1..].chars().all(|c| c.is_ascii_digit());
        if is_temp && dep_root_names.contains(name) {
            let start = name[1..].parse::<u32>().unwrap_or(0) + 1;
            let mut idx = start;
            let fresh = loop {
                let candidate = format!("t{idx}");
                if !cx.declared_names.contains(&candidate)
                    && !cx.options.unique_identifiers.contains(&candidate)
                {
                    break candidate;
                }
                idx += 1;
            };
            cx.decl_names.insert(*decl_id, fresh.clone());
            cx.declared_names.insert(fresh.clone());
            shifted_names.insert(*decl_id, fresh);
        }
    }

    // Collect scope variable declarations.
    // When enable_change_variable_codegen is active with deps, these are deferred
    // to emit after the change variable declarations.
    let mut scope_decl_stmts: Vec<ast::Statement<'a>> = Vec::new();
    for (name, id, decl_id) in &decl_names {
        let emit_name = shifted_names.get(decl_id).unwrap_or(name);
        if !cx.declared.contains(id) && !cx.declared_decl_ids.contains(decl_id) {
            cx.declared.insert(*id);
            cx.declared_decl_ids.insert(*decl_id);
            cx.declared_names.insert(emit_name.clone());
            cx.register_scoped_name(emit_name);
            let pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(emit_name));
            scope_decl_stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        pattern,
                        NONE,
                        None,
                        false,
                    )),
                    false,
                ),
            ));
        }
    }

    // Also declare reassignment targets.
    for reassign in &scope.reassignments {
        let id = reassign.id;
        let r_decl_id = reassign.declaration_id;
        if !cx.declared.contains(&id) && !cx.declared_decl_ids.contains(&r_decl_id) {
            if let Some(name) = reassign.name.as_ref() {
                let name_str = name.value().to_string();
                if cx.declared_names.contains(&name_str) {
                    // Already declared by name (e.g., from DeclareLocal). Track ids only.
                    cx.declared.insert(id);
                    cx.declared_decl_ids.insert(r_decl_id);
                } else {
                    cx.declared.insert(id);
                    cx.declared_decl_ids.insert(r_decl_id);
                    cx.declared_names.insert(name_str.clone());
                    cx.register_scoped_name(&name_str);
                    let pattern = cx
                        .builder
                        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(name.value()));
                    scope_decl_stmts.push(ast::Statement::VariableDeclaration(
                        cx.builder.alloc_variable_declaration(
                            SPAN,
                            ast::VariableDeclarationKind::Let,
                            cx.builder.vec1(cx.builder.variable_declarator(
                                SPAN,
                                ast::VariableDeclarationKind::Let,
                                pattern,
                                NONE,
                                None,
                                false,
                            )),
                            false,
                        ),
                    ));
                }
            } else {
                cx.declared.insert(id);
                cx.declared_decl_ids.insert(r_decl_id);
            }
        }
    }

    // Build dependency comparison: $[slot] !== dep_expr || ...
    // Sort dependencies by their rendered name (post-rename) to match upstream ordering.
    let mut sorted_deps: Vec<&ReactiveScopeDependency> = scope.dependencies.iter().collect();
    sorted_deps.sort_by_key(|a| dep_sort_key_with_cx(cx, a));

    // Extract optional-chain dependencies into temp variables.
    // The upstream compiler places PropertyLoad chains before the scope and
    // the scope depends on the resulting temp. Our pipeline places the
    // PropertyLoads before the scope too (already processed, results in cx.temps),
    // but the scope tracks a path dependency. We fix this by finding the
    // corresponding temp in cx.temps, giving it a name, and emitting a
    // declaration before the scope declarations.
    //
    // Note: DeriveMinimalDependenciesHIR may convert optional accesses to
    // unconditional when the hoistable tree says the base is NonNull, stripping
    // the `optional` flag from the dep path. In that case we also look for
    // temps whose source expression has optional chains that the dep path lost.
    let mut optional_chain_temps: HashMap<usize, String> = HashMap::new();
    for (dep_idx, dep) in sorted_deps.iter().enumerate() {
        let has_path_optional = dep.path.iter().any(|e| e.optional);
        let final_decl_id = if has_path_optional {
            find_temp_matching_dep(cx, dep)
        } else {
            // The dep path has no optional entries, but the original source
            // expression may have had optional chains that were stripped by
            // the hoistable-tree conversion. Try relaxed matching.
            find_temp_with_optional_chain_for_dep(cx, dep)
        };
        if let Some(final_decl_id) = final_decl_id
            && !body_has_any_property_loads(instructions)
        {
            let temp_name = alloc_fresh_temp_name(cx);
            if let Some(Some(dep_expr_ref)) = cx.temps.get(&final_decl_id) {
                let dep_expr = dep_expr_ref.clone_in(cx.allocator);
                let pattern = cx
                    .builder
                    .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&temp_name));
                stmts.push(ast::Statement::VariableDeclaration(
                    cx.builder.alloc_variable_declaration(
                        SPAN,
                        ast::VariableDeclarationKind::Const,
                        cx.builder.vec1(cx.builder.variable_declarator(
                            SPAN,
                            ast::VariableDeclarationKind::Const,
                            pattern,
                            NONE,
                            Some(dep_expr),
                            false,
                        )),
                        false,
                    ),
                ));
                // Replace the temp expression so body codegen uses the name.
                cx.temps
                    .insert(final_decl_id, Some(cx.ident_expr(&temp_name)));
                cx.declared_names.insert(temp_name.clone());
                optional_chain_temps.insert(dep_idx, temp_name);
            }
        }
    }

    // Emit scope declarations (after optional chain extractions).
    let defer_decls = cx.options.enable_change_variable_codegen && !scope.dependencies.is_empty();
    if !defer_decls {
        stmts.append(&mut scope_decl_stmts);
    }

    let deps: Vec<(u32, ast::Expression<'a>)> = sorted_deps
        .iter()
        .enumerate()
        .filter_map(|(i, dep)| {
            let dep_expr = if let Some(temp_name) = optional_chain_temps.get(&i) {
                cx.ident_expr(temp_name)
            } else {
                codegen_dependency_expr(cx, dep)?
            };
            let slot = cx.alloc_cache_slot();
            Some((slot, dep_expr))
        })
        .collect();

    // Allocate cache slots for outputs (using shifted names for dep-colliding declarations).
    let output_slots: Vec<(String, u32)> = decl_names
        .iter()
        .map(|(name, _, decl_id)| {
            let slot = cx.alloc_cache_slot();
            let final_name = shifted_names
                .get(decl_id)
                .cloned()
                .unwrap_or_else(|| name.clone());
            (final_name, slot)
        })
        .collect();

    // Deduplicate reassignments against declarations to avoid storing
    // the same variable in two separate cache slots.
    let output_decl_ids: HashSet<DeclarationId> = decl_names.iter().map(|(_, _, d)| *d).collect();
    let reassign_slots: Vec<(String, u32)> = scope
        .reassignments
        .iter()
        .filter_map(|reassign| {
            if output_decl_ids.contains(&reassign.declaration_id) {
                return None;
            }
            let name = reassign.name.as_ref()?.value().to_string();
            Some((name, cx.alloc_cache_slot()))
        })
        .collect();

    // Build the scope body.
    let mut body_stmts = codegen_block(cx, instructions);

    // Post-process: fuse test variables and setup statements into ternary
    // expressions to match upstream's ternary restructuring.
    fuse_scope_body_ternaries(cx, &mut body_stmts);

    // --- Change detection for debugging ---
    if cx.options.enable_change_detection_for_debugging && !deps.is_empty() {
        cx.needs_structural_check = true;
        let all_outputs: Vec<(String, u32)> = output_slots
            .iter()
            .chain(reassign_slots.iter())
            .cloned()
            .collect();

        // Build condition: $[0] !== dep0 || $[1] !== dep1 || ...
        let condition_expr = deps
            .iter()
            .map(|(slot, dep_expr)| {
                cx.builder.expression_binary(
                    SPAN,
                    cx.cache_access(*slot),
                    oxc_syntax::operator::BinaryOperator::StrictInequality,
                    dep_expr.clone_in(cx.allocator),
                )
            })
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        // Scope location string for diagnostics.
        let scope_loc = format_change_detection_scope_loc(scope, instructions);

        let mut block_stmts: Vec<ast::Statement<'a>> = Vec::new();

        // 1. Execute body unconditionally.
        block_stmts.extend(body_stmts);

        // 2. let condition = deps_test;
        let condition_pattern = cx
            .builder
            .binding_pattern_binding_identifier(SPAN, cx.builder.ident("condition"));
        block_stmts.push(ast::Statement::VariableDeclaration(
            cx.builder.alloc_variable_declaration(
                SPAN,
                ast::VariableDeclarationKind::Let,
                cx.builder.vec1(cx.builder.variable_declarator(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    condition_pattern,
                    NONE,
                    Some(condition_expr),
                    false,
                )),
                false,
            ),
        ));

        // 3. if (!condition) { let old$name = $[slot]; $structuralCheck(..., "cached", ...) }
        let mut cached_check_stmts: Vec<ast::Statement<'a>> = Vec::new();
        for (name, slot) in &all_outputs {
            let old_name = format!("old${name}");
            let old_pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&old_name));
            cached_check_stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Let,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Let,
                        old_pattern,
                        NONE,
                        Some(cx.cache_access(*slot)),
                        false,
                    )),
                    false,
                ),
            ));
            cached_check_stmts.push(build_structural_check_call(
                cx, &old_name, name, name, "cached", &scope_loc,
            ));
        }

        if !cached_check_stmts.is_empty() {
            block_stmts.push(
                cx.builder.statement_if(
                    SPAN,
                    cx.builder.expression_unary(
                        SPAN,
                        oxc_syntax::operator::UnaryOperator::LogicalNot,
                        cx.ident_expr("condition"),
                    ),
                    cx.builder
                        .statement_block(SPAN, cx.builder.vec_from_iter(cached_check_stmts)),
                    None,
                ),
            );
        }

        // 4. Store deps and outputs.
        for (slot, dep_expr) in deps {
            block_stmts.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
        for (name, slot) in &all_outputs {
            block_stmts.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        // 5. if (condition) { body_again; $structuralCheck(..., "recomputed", ...); name = $[slot] }
        // Re-generate the body for the recomputed path.
        let recompute_body = codegen_block(cx, instructions);
        let mut recompute_stmts: Vec<ast::Statement<'a>> = Vec::new();
        recompute_stmts.extend(recompute_body);
        for (name, slot) in &all_outputs {
            recompute_stmts.push(build_structural_check_call(
                cx,
                &format!("$[{slot}]"),
                name,
                name,
                "recomputed",
                &scope_loc,
            ));
            recompute_stmts.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        block_stmts.push(
            cx.builder.statement_if(
                SPAN,
                cx.ident_expr("condition"),
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(recompute_stmts)),
                None,
            ),
        );

        // Wrap in a block statement.
        stmts.push(
            cx.builder
                .statement_block(SPAN, cx.builder.vec_from_iter(block_stmts)),
        );
        return stmts;
    }

    if deps.is_empty() {
        // Zero-dependency: use sentinel check.
        let sentinel_slot = if !output_slots.is_empty() {
            output_slots[0].1
        } else if !reassign_slots.is_empty() {
            reassign_slots[0].1
        } else {
            cx.alloc_cache_slot()
        };

        let mut test = cx.builder.expression_binary(
            SPAN,
            cx.cache_access(sentinel_slot),
            oxc_syntax::operator::BinaryOperator::StrictEquality,
            cx.sentinel_expr(),
        );

        // disable_memoization_for_debugging: always recompute (append `|| true`)
        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

        let mut else_body = Vec::new();
        for (name, slot) in &output_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }
        for (name, slot) in &reassign_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        let mut if_body = body_stmts;
        for (name, slot) in &output_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }
        for (name, slot) in &reassign_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        let else_stmt = if else_body.is_empty() {
            None
        } else {
            Some(
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(else_body)),
            )
        };

        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(if_body)),
                else_stmt,
            ),
        );
    } else if cx.options.enable_change_variable_codegen {
        // Change variable codegen: emit explicit comparison variables.
        // `const c_0 = $[0] !== dep0; const c_1 = $[1] !== dep1; if (c_0 || c_1) { ... }`
        let mut change_var_names: Vec<String> = Vec::new();
        for (i, (slot, dep_expr)) in deps.iter().enumerate() {
            let var_name = cx.synthesize_name(&format!("c_{i}"));
            let comparison = cx.builder.expression_binary(
                SPAN,
                cx.cache_access(*slot),
                oxc_syntax::operator::BinaryOperator::StrictInequality,
                dep_expr.clone_in(cx.allocator),
            );
            let pattern = cx
                .builder
                .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&var_name));
            stmts.push(ast::Statement::VariableDeclaration(
                cx.builder.alloc_variable_declaration(
                    SPAN,
                    ast::VariableDeclarationKind::Const,
                    cx.builder.vec1(cx.builder.variable_declarator(
                        SPAN,
                        ast::VariableDeclarationKind::Const,
                        pattern,
                        NONE,
                        Some(comparison),
                        false,
                    )),
                    false,
                ),
            ));
            change_var_names.push(var_name);
        }

        // Emit deferred scope declarations after change variables.
        stmts.extend(scope_decl_stmts);

        let mut test = change_var_names
            .iter()
            .map(|name| cx.ident_expr(name))
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

        // If body: recompute + store deps + store outputs.
        let mut if_body = body_stmts;
        for (slot, dep_expr) in deps {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
        for (name, slot) in &output_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }
        for (name, slot) in &reassign_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        // Else body: load cached values.
        let mut else_body = Vec::new();
        for (name, slot) in &output_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }
        for (name, slot) in &reassign_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        let else_stmt = if else_body.is_empty() {
            None
        } else {
            Some(
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(else_body)),
            )
        };

        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(if_body)),
                else_stmt,
            ),
        );
    } else {
        // Multi-dependency: compare deps.
        // We need dep expressions twice (test + store), so clone for the test.
        let mut test_parts: Vec<ast::Expression<'a>> = Vec::new();
        for (slot, dep_expr) in &deps {
            test_parts.push(cx.builder.expression_binary(
                SPAN,
                cx.cache_access(*slot),
                oxc_syntax::operator::BinaryOperator::StrictInequality,
                dep_expr.clone_in(cx.allocator),
            ));
        }

        let mut test = test_parts
            .into_iter()
            .reduce(|left, right| {
                cx.builder.expression_logical(
                    SPAN,
                    left,
                    oxc_syntax::operator::LogicalOperator::Or,
                    right,
                )
            })
            .unwrap();

        // disable_memoization_for_debugging: always recompute (append `|| true`)
        if cx.options.disable_memoization_for_debugging {
            test = cx.builder.expression_logical(
                SPAN,
                test,
                LogicalOperator::Or,
                cx.builder.expression_boolean_literal(SPAN, true),
            );
        }

        // If body: recompute + store deps + store outputs.
        let mut if_body = body_stmts;
        // Store dependency values (consume deps here since this is the last use).
        for (slot, dep_expr) in deps {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(slot, dep_expr)),
            );
        }
        // Store outputs.
        for (name, slot) in &output_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }
        for (name, slot) in &reassign_slots {
            if_body.push(
                cx.builder
                    .statement_expression(SPAN, cx.cache_assign(*slot, cx.ident_expr(name))),
            );
        }

        // Else body: load cached values.
        let mut else_body = Vec::new();
        for (name, slot) in &output_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }
        for (name, slot) in &reassign_slots {
            else_body.push(
                cx.builder.statement_expression(
                    SPAN,
                    cx.builder.expression_assignment(
                        SPAN,
                        AssignmentOperator::Assign,
                        ast::AssignmentTarget::from(
                            cx.builder
                                .simple_assignment_target_assignment_target_identifier(
                                    SPAN,
                                    cx.builder.ident(name),
                                ),
                        ),
                        cx.cache_access(*slot),
                    ),
                ),
            );
        }

        let else_stmt = if else_body.is_empty() {
            None
        } else {
            Some(
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(else_body)),
            )
        };

        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder
                    .statement_block(SPAN, cx.builder.vec_from_iter(if_body)),
                else_stmt,
            ),
        );
    }

    // Handle early return value.
    if let Some(early_return) = &scope.early_return_value {
        let name = early_return
            .value
            .name
            .as_ref()
            .map(|n| n.value().to_string())
            .unwrap_or_else(|| format!("t{}", early_return.value.id.0));
        let test = cx.builder.expression_binary(
            SPAN,
            cx.ident_expr(&name),
            oxc_syntax::operator::BinaryOperator::StrictInequality,
            cx.early_return_sentinel_expr(),
        );
        stmts.push(
            cx.builder.statement_if(
                SPAN,
                test,
                cx.builder.statement_block(
                    SPAN,
                    cx.builder.vec1(
                        cx.builder
                            .statement_return(SPAN, Some(cx.ident_expr(&name))),
                    ),
                ),
                None,
            ),
        );
    }

    stmts
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn codegen_dependency_expr<'a>(
    cx: &mut CodegenContext<'a>,
    dep: &ReactiveScopeDependency,
) -> Option<ast::Expression<'a>> {
    let name = dep.identifier.name.as_ref()?.value();
    let mut expr = cx.ident_expr(name);
    for entry in &dep.path {
        // Use computed member access for numeric property names (e.g., x[0][1])
        // and static member access for identifier-like names (e.g., x.foo).
        let is_numeric = entry.property.parse::<f64>().is_ok();
        if is_numeric {
            let num_val = entry.property.parse::<f64>().unwrap();
            expr = ast::Expression::from(
                cx.builder.member_expression_computed(
                    SPAN,
                    expr,
                    cx.builder
                        .expression_numeric_literal(SPAN, num_val, None, NumberBase::Decimal),
                    entry.optional,
                ),
            );
        } else if entry.optional {
            expr = ast::Expression::from(
                cx.builder.member_expression_static(
                    SPAN,
                    expr,
                    cx.builder
                        .identifier_name(SPAN, cx.builder.ident(entry.property.as_str())),
                    true,
                ),
            );
        } else {
            expr = ast::Expression::from(
                cx.builder.member_expression_static(
                    SPAN,
                    expr,
                    cx.builder
                        .identifier_name(SPAN, cx.builder.ident(entry.property.as_str())),
                    false,
                ),
            );
        }
    }
    Some(expr)
}

fn codegen_arguments<'a>(
    cx: &mut CodegenContext<'a>,
    args: &[Argument],
) -> Option<oxc_allocator::Vec<'a, ast::Argument<'a>>> {
    let mut lowered = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Argument::Place(place) => {
                lowered.push(ast::Argument::from(codegen_place(cx, place)?));
            }
            Argument::Spread(place) => {
                lowered.push(
                    cx.builder
                        .argument_spread_element(SPAN, codegen_place(cx, place)?),
                );
            }
        }
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_method_call_callee<'a>(
    cx: &mut CodegenContext<'a>,
    receiver: &Place,
    property: &Place,
    receiver_optional: bool,
) -> Option<ast::Expression<'a>> {
    let prop_expr = codegen_place(cx, property)?;

    // If the property already resolved to a member expression (e.g., x.push
    // from a PropertyLoad temp), use it directly as the callee — the receiver
    // binding is already embedded in the member expression.
    if matches!(
        &prop_expr,
        ast::Expression::StaticMemberExpression(_) | ast::Expression::ComputedMemberExpression(_)
    ) {
        return Some(prop_expr);
    }

    // If the property is a string literal that is a valid identifier,
    // build a static member expression (x.push instead of x["push"]).
    if let ast::Expression::StringLiteral(lit) = &prop_expr
        && is_identifier_name(&lit.value)
    {
        return Some(ast::Expression::from(
            cx.builder.member_expression_static(
                SPAN,
                codegen_place(cx, receiver)?,
                cx.builder
                    .identifier_name(SPAN, cx.builder.ident(lit.value.as_str())),
                receiver_optional,
            ),
        ));
    }

    // Default: computed member expression.
    Some(ast::Expression::from(
        cx.builder.member_expression_computed(
            SPAN,
            codegen_place(cx, receiver)?,
            prop_expr,
            receiver_optional,
        ),
    ))
}

fn codegen_array_elements<'a>(
    cx: &mut CodegenContext<'a>,
    elements: &[ArrayElement],
) -> Option<oxc_allocator::Vec<'a, ast::ArrayExpressionElement<'a>>> {
    let mut lowered = Vec::with_capacity(elements.len());
    for element in elements {
        let el = match element {
            ArrayElement::Place(place) => {
                ast::ArrayExpressionElement::from(codegen_place(cx, place)?)
            }
            ArrayElement::Spread(place) => cx
                .builder
                .array_expression_element_spread_element(SPAN, codegen_place(cx, place)?),
            ArrayElement::Hole => cx.builder.array_expression_element_elision(SPAN),
        };
        lowered.push(el);
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_object_properties<'a>(
    cx: &mut CodegenContext<'a>,
    properties: &[ObjectPropertyOrSpread],
) -> Option<oxc_allocator::Vec<'a, ast::ObjectPropertyKind<'a>>> {
    let mut lowered = Vec::with_capacity(properties.len());
    for property in properties {
        let prop = match property {
            ObjectPropertyOrSpread::Spread(place) => cx
                .builder
                .object_property_kind_spread_property(SPAN, codegen_place(cx, place)?),
            ObjectPropertyOrSpread::Property(property) => {
                let value = codegen_place(cx, &property.place)?;
                let (key, shorthand, computed) =
                    codegen_object_property_key(cx, &property.key, &value)?;
                cx.builder.object_property_kind_object_property(
                    SPAN,
                    ast::PropertyKind::Init,
                    key,
                    value,
                    property.type_ == ObjectPropertyType::Method,
                    shorthand,
                    computed,
                )
            }
        };
        lowered.push(prop);
    }
    Some(cx.builder.vec_from_iter(lowered))
}

fn codegen_object_property_key<'a>(
    cx: &mut CodegenContext<'a>,
    key: &ObjectPropertyKey,
    value: &ast::Expression<'a>,
) -> Option<(ast::PropertyKey<'a>, bool, bool)> {
    match key {
        ObjectPropertyKey::Identifier(name) => {
            let shorthand = matches!(
                value,
                ast::Expression::Identifier(identifier) if identifier.name == name.as_str()
            );
            Some((
                cx.builder
                    .property_key_static_identifier(SPAN, cx.builder.ident(name)),
                shorthand,
                false,
            ))
        }
        ObjectPropertyKey::String(name) if is_identifier_name(name) => Some((
            cx.builder
                .property_key_static_identifier(SPAN, cx.builder.ident(name)),
            false,
            false,
        )),
        ObjectPropertyKey::String(name) => Some((
            ast::PropertyKey::from(cx.builder.expression_string_literal(
                SPAN,
                cx.builder.atom(name),
                None,
            )),
            false,
            false,
        )),
        ObjectPropertyKey::Number(val) => Some((
            ast::PropertyKey::from(cx.builder.expression_numeric_literal(
                SPAN,
                *val,
                None,
                NumberBase::Decimal,
            )),
            false,
            false,
        )),
        ObjectPropertyKey::Computed(place) => Some((
            ast::PropertyKey::from(codegen_place(cx, place)?),
            false,
            true,
        )),
    }
}

fn all_pattern_vars_declared(cx: &CodegenContext<'_>, pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Array(arr) => arr.items.iter().all(|item| match item {
            ArrayElement::Place(p) | ArrayElement::Spread(p) => {
                let name = identifier_name(&p.identifier);
                cx.declared_names.contains(&name)
            }
            ArrayElement::Hole => true,
        }),
        Pattern::Object(obj) => obj.properties.iter().all(|prop| match prop {
            ObjectPropertyOrSpread::Property(p) => {
                let name = identifier_name(&p.place.identifier);
                cx.declared_names.contains(&name)
            }
            ObjectPropertyOrSpread::Spread(p) => {
                let name = identifier_name(&p.identifier);
                cx.declared_names.contains(&name)
            }
        }),
    }
}

/// Try to inline a zero-dependency (sentinel) scope by storing its output
/// expression in the temp map. Returns true if the scope was inlined (should
/// be skipped in output), false if it should be emitted normally.
/// Build a sort key for a scope dependency (for deterministic ordering).
/// Sort key using the codegen context's name resolution (post-rename).
/// This ensures temp deps sort by their rendered name (t0, t1, ...)
/// rather than their pre-rename internal ID.
fn dep_sort_key_with_cx(cx: &CodegenContext, dep: &ReactiveScopeDependency) -> String {
    let root = if let Some(name) = cx.decl_names.get(&dep.identifier.declaration_id) {
        name.clone()
    } else {
        dep.identifier
            .name
            .as_ref()
            .map(|n| n.value().to_string())
            .unwrap_or_else(|| format!("t{}", dep.identifier.id.0))
    };
    if dep.path.is_empty() {
        return root;
    }
    let mut key = root;
    for entry in &dep.path {
        key.push('.');
        key.push_str(&entry.property);
    }
    key
}

/// Extract the collection Place from a for-of/for-in init block by scanning
/// for `IteratorNext` or `NextPropertyOf` instructions.
fn extract_for_collection(init: &ReactiveBlock) -> Option<Place> {
    for stmt in init {
        let ReactiveStatement::Instruction(instr) = stmt else {
            continue;
        };
        match &instr.value {
            InstructionValue::IteratorNext { collection, .. } => {
                return Some(collection.clone());
            }
            InstructionValue::NextPropertyOf { value, .. } => {
                return Some(value.clone());
            }
            _ => {}
        }
    }
    None
}

/// Extract the for-of/for-in left-hand side from processed init statements.
/// Handles both `VariableDeclaration` (new binding) and assignment expressions
/// (pre-declared loop variable).
fn extract_for_of_left<'a>(
    cx: &mut CodegenContext<'a>,
    init_stmts: Vec<ast::Statement<'a>>,
) -> Option<ast::ForStatementLeft<'a>> {
    for s in init_stmts {
        match s {
            ast::Statement::VariableDeclaration(mut decl) => {
                // Strip initializer — for-of/for-in left side is pattern only.
                for declarator in decl.declarations.iter_mut() {
                    declarator.init = None;
                }
                return Some(ast::ForStatementLeft::VariableDeclaration(decl));
            }
            ast::Statement::ExpressionStatement(expr_stmt) => {
                let expr = expr_stmt.unbox().expression;
                if let Some(target) = super::super::codegen_backend::hir_to_ast::expression_to_simple_assignment_target(cx.builder, expr).map(ast::AssignmentTarget::from) {
                    return Some(ast::ForStatementLeft::from(target));
                }
            }
            _ => {}
        }
    }
    None
}

/// Mark all terminal labels in a reactive block as implicit, preventing
/// them from being emitted as JavaScript labels. Used for inner function
/// expressions where CFG block IDs should not become visible labels.
fn suppress_labels_recursive(block: &mut ReactiveBlock) {
    for stmt in block.iter_mut() {
        match stmt {
            ReactiveStatement::Terminal(term_stmt) => {
                if let Some(label) = &mut term_stmt.label {
                    label.implicit = true;
                }
                match &mut term_stmt.terminal {
                    ReactiveTerminal::If {
                        consequent,
                        alternate,
                        ..
                    } => {
                        suppress_labels_recursive(consequent);
                        if let Some(alt) = alternate {
                            suppress_labels_recursive(alt);
                        }
                    }
                    ReactiveTerminal::Switch { cases, .. } => {
                        for case in cases {
                            if let Some(block) = &mut case.block {
                                suppress_labels_recursive(block);
                            }
                        }
                    }
                    ReactiveTerminal::For {
                        init, loop_block, ..
                    } => {
                        suppress_labels_recursive(init);
                        suppress_labels_recursive(loop_block);
                    }
                    ReactiveTerminal::While { loop_block, .. }
                    | ReactiveTerminal::DoWhile { loop_block, .. }
                    | ReactiveTerminal::ForOf { loop_block, .. }
                    | ReactiveTerminal::ForIn { loop_block, .. } => {
                        suppress_labels_recursive(loop_block);
                    }
                    ReactiveTerminal::Try {
                        block: try_block,
                        handler,
                        ..
                    } => {
                        suppress_labels_recursive(try_block);
                        suppress_labels_recursive(handler);
                    }
                    _ => {}
                }
            }
            ReactiveStatement::Scope(scope_block) => {
                suppress_labels_recursive(&mut scope_block.instructions);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                suppress_labels_recursive(&mut pruned.instructions);
            }
            _ => {}
        }
    }
}

fn crate_label_name(block_id: BlockId) -> &'static str {
    // Leak a string for the label name. In production, this would use an arena.
    let s = format!("bb{}", block_id.0);
    Box::leak(s.into_boxed_str())
}

// ---------------------------------------------------------------------------
// Operator helpers (reused from hir_to_ast)
// ---------------------------------------------------------------------------

fn lower_primitive<'a>(builder: AstBuilder<'a>, value: &PrimitiveValue) -> ast::Expression<'a> {
    super::super::codegen_backend::hir_to_ast::lower_primitive(builder, value)
}

fn lower_binary_operator(op: crate::hir::types::BinaryOperator) -> BinaryOperator {
    super::super::codegen_backend::hir_to_ast::lower_binary_operator(op)
}

fn lower_unary_operator(
    op: crate::hir::types::UnaryOperator,
) -> oxc_syntax::operator::UnaryOperator {
    super::super::codegen_backend::hir_to_ast::lower_unary_operator(op)
}

fn lower_logical_operator(op: crate::hir::types::LogicalOperator) -> LogicalOperator {
    super::super::codegen_backend::hir_to_ast::lower_logical_operator(op)
}

fn lower_update_operator(
    op: crate::hir::types::UpdateOperator,
) -> oxc_syntax::operator::UpdateOperator {
    super::super::codegen_backend::hir_to_ast::lower_update_operator(op)
}

#[allow(dead_code)]
fn dump_reactive_block(block: &ReactiveBlock, indent: usize) {
    let pad = "  ".repeat(indent);
    for stmt in block.iter() {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                let lv = instr
                    .lvalue
                    .as_ref()
                    .map(|p| identifier_name(&p.identifier))
                    .unwrap_or_default();
                eprintln!(
                    "{pad}Instr({lv}): {:?}",
                    std::mem::discriminant(&instr.value)
                );
            }
            ReactiveStatement::Terminal(term) => {
                eprintln!(
                    "{pad}Terminal: {:?}",
                    std::mem::discriminant(&term.terminal)
                );
            }
            ReactiveStatement::Scope(scope) => {
                let decls: Vec<String> = scope
                    .scope
                    .declarations
                    .values()
                    .map(|d| {
                        d.identifier
                            .name
                            .as_ref()
                            .map(|n| n.value().to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                eprintln!("{pad}Scope(decls={decls:?}):");
                dump_reactive_block(&scope.instructions, indent + 1);
            }
            ReactiveStatement::PrunedScope(pruned) => {
                eprintln!("{pad}PrunedScope:");
                dump_reactive_block(&pruned.instructions, indent + 1);
            }
        }
    }
}

/// Reconstruct a for-loop init from the emitted statements.
///
/// The HIR may split `let i = expr` into separate DeclareLocal (`let i;`) and
/// StoreLocal (`i = expr;`) instructions.  This function merges them back:
///
/// - `[let i;, i = expr;]` → `let i = expr`
/// - `[let i = 0;, let len = expr;]` → `let i = 0, len = expr`
/// - `[let i;]` → `let i`
///
/// Returns `None` when the statements cannot be reconstructed into a single
/// `ForStatementInit`.
fn reconstruct_for_init<'a>(
    cx: &mut CodegenContext<'a>,
    stmts: Vec<ast::Statement<'a>>,
) -> Option<ast::ForStatementInit<'a>> {
    if stmts.is_empty() {
        return None;
    }

    // Fast path: single statement.
    if stmts.len() == 1 {
        let stmt = stmts.into_iter().next().unwrap();
        match stmt {
            ast::Statement::VariableDeclaration(decl) => {
                return Some(ast::ForStatementInit::VariableDeclaration(decl));
            }
            // Single assignment: convert `VAR = EXPR;` to `let VAR = EXPR`
            // for for-init position when the variable was already hoisted by
            // scope declarations.
            ast::Statement::ExpressionStatement(es) => {
                if let ast::Expression::AssignmentExpression(assign) = es.unbox().expression
                    && assign.operator == AssignmentOperator::Assign
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(ref ident) =
                        assign.left
                {
                    let name = ident.name.to_string();
                    let pattern = cx
                        .builder
                        .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&name));
                    let kind = ast::VariableDeclarationKind::Let;
                    return Some(ast::ForStatementInit::VariableDeclaration(
                        cx.builder.alloc_variable_declaration(
                            SPAN,
                            kind,
                            cx.builder.vec1(cx.builder.variable_declarator(
                                SPAN,
                                kind,
                                pattern,
                                NONE,
                                Some(assign.unbox().right),
                                false,
                            )),
                            false,
                        ),
                    ));
                }
                return None;
            }
            _ => return None,
        }
    }

    // Multi-statement path: merge DeclareLocal + assignment pairs.
    // Collect all declarators from variable declarations, then merge in
    // assignments for uninitialized ones.
    struct DeclInfo<'a> {
        name: String,
        id: ast::BindingPattern<'a>,
        init: Option<ast::Expression<'a>>,
    }
    let mut declarators: Vec<DeclInfo<'a>> = Vec::new();
    let mut kind = ast::VariableDeclarationKind::Let;

    for stmt in stmts {
        match stmt {
            ast::Statement::VariableDeclaration(mut decl) => {
                // Use Let if any declarator uses Let (it means the variable is reassigned)
                if decl.kind == ast::VariableDeclarationKind::Let {
                    kind = ast::VariableDeclarationKind::Let;
                }
                for d in decl.declarations.drain(..) {
                    let name = match &d.id {
                        ast::BindingPattern::BindingIdentifier(id) => id.name.to_string(),
                        _ => String::new(),
                    };
                    declarators.push(DeclInfo {
                        name,
                        id: d.id,
                        init: d.init,
                    });
                }
            }
            ast::Statement::ExpressionStatement(es) => {
                if let ast::Expression::AssignmentExpression(assign) = es.unbox().expression
                    && assign.operator == AssignmentOperator::Assign
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(ident) = &assign.left
                {
                    let name = ident.name.as_str();
                    // Find the matching uninitialised declarator and set its init.
                    if let Some(d) = declarators
                        .iter_mut()
                        .rev()
                        .find(|d| d.init.is_none() && d.name == name)
                    {
                        d.init = Some(assign.unbox().right);
                        continue;
                    }
                }
                // Could not merge — bail out.
                return None;
            }
            _ => return None,
        }
    }

    if declarators.is_empty() {
        return None;
    }

    let mut oxc_declarators = cx.builder.vec_with_capacity(declarators.len());
    for d in declarators {
        oxc_declarators.push(
            cx.builder
                .variable_declarator(SPAN, kind, d.id, NONE, d.init, false),
        );
    }
    Some(ast::ForStatementInit::VariableDeclaration(
        cx.builder
            .alloc_variable_declaration(SPAN, kind, oxc_declarators, false),
    ))
}

// ---------------------------------------------------------------------------
// Change detection helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Ternary fusion post-processing
// ---------------------------------------------------------------------------

/// Detect and fuse patterns like:
/// ```js
/// let t0 = TEST;
/// SETUP;
/// t0 ? CONSEQUENT : ALT;
/// ```
/// into:
/// ```js
/// TEST ? ((SETUP_EXPR), CONSEQUENT) : ALT;
/// ```
/// This matches the upstream string codegen's ternary restructuring.
fn fuse_scope_body_ternaries<'a>(cx: &mut CodegenContext<'a>, stmts: &mut Vec<ast::Statement<'a>>) {
    // Look for the pattern: const/let TEMP = EXPR; ... TEMP ? A : B;
    // where TEMP is only used as the ternary test.
    let mut i = 0;
    while i < stmts.len() {
        // Find a variable declaration `let tN = EXPR;`
        let test_name = match &stmts[i] {
            ast::Statement::VariableDeclaration(decl)
                if decl.declarations.len() == 1
                    && decl.declarations[0].init.is_some()
                    && matches!(
                        &decl.declarations[0].id,
                        ast::BindingPattern::BindingIdentifier(_)
                    ) =>
            {
                let d = &decl.declarations[0];
                let name = if let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                    id.name.to_string()
                } else {
                    i += 1;
                    continue;
                };
                // Only match temp-like names (t0, t1, etc.)
                if !name.starts_with('t') || !name[1..].chars().all(|c| c.is_ascii_digit()) {
                    i += 1;
                    continue;
                }
                name
            }
            _ => {
                i += 1;
                continue;
            }
        };

        // Look ahead for setup statements (expressions and temp declarations)
        // and a ternary using test_name.
        let mut ternary_idx = None;
        for (j, stmt) in stmts.iter().enumerate().skip(i + 1) {
            if let ast::Statement::ExpressionStatement(es) = stmt
                && let ast::Expression::ConditionalExpression(cond) = &es.expression
                && let ast::Expression::Identifier(id) = &cond.test
                && id.name.as_str() == test_name
            {
                ternary_idx = Some(j);
                break;
            } else if matches!(
                stmt,
                ast::Statement::ExpressionStatement(_) | ast::Statement::VariableDeclaration(_)
            ) {
                continue; // Expression statements and temp declarations are OK
            } else {
                break; // Other statement types break the pattern
            }
        }

        let Some(ternary_pos) = ternary_idx else {
            i += 1;
            continue;
        };

        // Extract the test expression from the declaration.
        let test_decl_stmt = stmts.remove(i);
        let test_init = if let ast::Statement::VariableDeclaration(mut decl) = test_decl_stmt {
            decl.declarations.drain(..).next().and_then(|d| d.init)
        } else {
            None
        };
        let Some(real_test) = test_init else {
            i += 1;
            continue;
        };

        // Collect setup expressions between the declaration and the ternary.
        // Also handle temp variable declarations (`let t1 = EXPR;`) by
        // converting them to assignment expressions for sequence fusion.
        let setup_count = ternary_pos - 1 - i; // positions shifted after remove
        let mut setup_exprs: Vec<ast::Expression<'a>> = Vec::new();
        let mut temp_decls: HashMap<String, ast::Expression<'a>> = HashMap::new();
        for _ in 0..setup_count {
            let setup_stmt = stmts.remove(i);
            match setup_stmt {
                ast::Statement::ExpressionStatement(es) => {
                    setup_exprs.push(es.unbox().expression);
                }
                ast::Statement::VariableDeclaration(mut decl) => {
                    // Convert `let tN = EXPR` to an assignment expression and
                    // record the temp for later inlining into the ternary.
                    for d in decl.declarations.drain(..) {
                        if let ast::BindingPattern::BindingIdentifier(id) = &d.id {
                            let name = id.name.to_string();
                            if let Some(init) = d.init {
                                temp_decls.insert(name.clone(), init.clone_in(cx.allocator));
                                // Emit as assignment expression for sequence.
                                let assign = cx.builder.expression_assignment(
                                    SPAN,
                                    AssignmentOperator::Assign,
                                    ast::AssignmentTarget::from(
                                        cx.builder
                                            .simple_assignment_target_assignment_target_identifier(
                                                SPAN,
                                                cx.builder.ident(&name),
                                            ),
                                    ),
                                    init,
                                );
                                setup_exprs.push(assign);
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Now stmts[i] is the ternary statement. Replace the test.
        if let ast::Statement::ExpressionStatement(es) = &mut stmts[i]
            && let ast::Expression::ConditionalExpression(cond) = &mut es.expression
        {
            // Replace test identifier with the real test expression.
            cond.test = real_test;

            // Check if the consequent is a temp identifier that was declared in setup.
            // Pattern: `x = []; let t1 = EXPR; x = []; t0 ? t1 : ALT`
            // → split setup at t1 decl, inline t1, setup1 → consequent, setup2 → alternate
            let cons_temp_name = if let ast::Expression::Identifier(id) = &cond.consequent {
                let n = id.name.to_string();
                if n.starts_with('t') && n.len() > 1 && n[1..].chars().all(|c| c.is_ascii_digit()) {
                    Some(n)
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(ref temp_name) = cons_temp_name
                && let Some(temp_expr) = temp_decls.get(temp_name)
            {
                // The consequent is a temp reference. Replace it with the temp's value.
                cond.consequent = temp_expr.clone_in(cx.allocator);

                // Split setup_exprs at the temp's assignment.
                // Everything before (including the temp assignment) → consequent setup
                // Everything after → alternate setup
                let split_pos = setup_exprs
                    .iter()
                    .position(|expr| {
                        if let ast::Expression::AssignmentExpression(assign) = expr
                            && let ast::AssignmentTarget::AssignmentTargetIdentifier(id) =
                                &assign.left
                        {
                            id.name.as_str() == temp_name.as_str()
                        } else {
                            false
                        }
                    })
                    .map(|p| p + 1)
                    .unwrap_or(setup_exprs.len());

                let alt_setup: Vec<ast::Expression<'a>> = setup_exprs.drain(split_pos..).collect();
                // Remove the temp assignment itself from consequent setup
                // (it's been inlined into the consequent)
                if let Some(ast::Expression::AssignmentExpression(assign)) = setup_exprs.last()
                    && let ast::AssignmentTarget::AssignmentTargetIdentifier(id) = &assign.left
                    && id.name.as_str() == temp_name.as_str()
                {
                    setup_exprs.pop();
                }

                // Build consequent sequence: (setup1, CONS_EXPR)
                if !setup_exprs.is_empty() {
                    let cons = std::mem::replace(
                        &mut cond.consequent,
                        cx.builder.expression_null_literal(SPAN),
                    );
                    setup_exprs.push(cons);
                    cond.consequent = cx
                        .builder
                        .expression_sequence(SPAN, cx.builder.vec_from_iter(setup_exprs));
                }

                // Build alternate sequence: (setup2, ALT_EXPR)
                if !alt_setup.is_empty() {
                    let alt = std::mem::replace(
                        &mut cond.alternate,
                        cx.builder.expression_null_literal(SPAN),
                    );
                    let mut alt_seq = alt_setup;
                    alt_seq.push(alt);
                    cond.alternate = cx
                        .builder
                        .expression_sequence(SPAN, cx.builder.vec_from_iter(alt_seq));
                }

                // Skip normal fusion since we handled it here.
                // Don't increment i — process same position again.
                continue;
            }

            // Fuse setup expressions into the consequent as a sequence.
            if !setup_exprs.is_empty() {
                let original_consequent = std::mem::replace(
                    &mut cond.consequent,
                    cx.builder.expression_null_literal(SPAN),
                );
                setup_exprs.push(original_consequent);
                cond.consequent = cx
                    .builder
                    .expression_sequence(SPAN, cx.builder.vec_from_iter(setup_exprs));
            }
        }

        // Don't increment i — process the same position again in case of chaining.
    }
}

const STRUCTURAL_CHECK_IDENT: &str = "$structuralCheck";

/// Format the scope location string for $structuralCheck diagnostics.
/// Returns "(startLine:endLine)" based on scope declaration/instruction locations.
fn format_change_detection_scope_loc(
    scope: &ReactiveScope,
    instructions: &ReactiveBlock,
) -> String {
    let mut lines: Vec<u32> = Vec::new();
    for decl in scope.declarations.values() {
        if let SourceLocation::Source(range) = &decl.identifier.loc {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
    for reassign in &scope.reassignments {
        if let SourceLocation::Source(range) = &reassign.loc {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
    // Also include instruction locations.
    collect_block_lines(instructions, &mut lines);
    if lines.is_empty() {
        "unknown location".to_string()
    } else {
        let min = lines.iter().min().unwrap();
        let max = lines.iter().max().unwrap();
        format!("({min}:{max})")
    }
}

fn collect_block_lines(block: &ReactiveBlock, lines: &mut Vec<u32>) {
    for stmt in block {
        if let ReactiveStatement::Instruction(instr) = stmt
            && let SourceLocation::Source(range) = &instr.loc
        {
            lines.push(range.start.line);
            lines.push(range.end.line);
        }
    }
}

/// Build a `$structuralCheck(old, new, "name", "fnName", "phase", "loc")` call statement.
fn build_structural_check_call<'a>(
    cx: &mut CodegenContext<'a>,
    old_expr_name: &str,
    new_expr_name: &str,
    var_name: &str,
    phase: &str,
    loc: &str,
) -> ast::Statement<'a> {
    let fn_name = cx.fn_name.clone();
    let old_expr = if old_expr_name.starts_with("$[") {
        // Cache access expression like $[0]
        let slot: u32 = old_expr_name[2..old_expr_name.len() - 1]
            .parse()
            .unwrap_or(0);
        cx.cache_access(slot)
    } else {
        cx.ident_expr(old_expr_name)
    };
    let new_expr = cx.ident_expr(new_expr_name);

    let mut args = cx.builder.vec_with_capacity(6);
    args.push(ast::Argument::from(old_expr));
    args.push(ast::Argument::from(new_expr));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(var_name),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(&fn_name),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(phase),
        None,
    )));
    args.push(ast::Argument::from(cx.builder.expression_string_literal(
        SPAN,
        cx.builder.atom(loc),
        None,
    )));

    cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(STRUCTURAL_CHECK_IDENT)),
            NONE,
            args,
            false,
        ),
    )
}

// ---------------------------------------------------------------------------
// Function expression fallback via ReactiveFunction
// ---------------------------------------------------------------------------

/// Build a function expression by constructing a ReactiveFunction from the HIR
/// and using codegen_ast's own codegen. Handles cases where hir_to_ast fails.
fn lower_function_expression_via_reactive<'a>(
    cx: &mut CodegenContext<'a>,
    name: Option<&str>,
    lowered_func: &crate::hir::types::LoweredFunction,
    expr_type: FunctionExpressionType,
) -> Option<ast::Expression<'a>> {
    let hir_func = &lowered_func.func;
    let mut reactive_fn =
        crate::reactive_scopes::build_reactive_function::build_reactive_function(hir_func.clone());

    // Run the same post-reactive passes as the string codegen:
    // - prune_unused_labels: remove unreferenced labels
    // - prune_unused_lvalues: set lvalue=None on instructions whose temp
    //   result is never referenced, so they emit as expression statements
    //   instead of being silently inlined into the temp map
    // - prune_hoisted_contexts: remove unnecessary context hoisting
    crate::reactive_scopes::prune_unused_labels_reactive::prune_unused_labels(&mut reactive_fn);
    crate::reactive_scopes::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn);
    let _ =
        crate::reactive_scopes::prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn);

    // Mark all terminal labels as implicit for inner functions — these are
    // CFG block IDs that shouldn't be emitted as visible JavaScript labels.
    suppress_labels_recursive(&mut reactive_fn.body);

    let options = CodegenOptions {
        enable_change_variable_codegen: false,
        enable_emit_hook_guards: false,
        enable_change_detection_for_debugging: false,
        enable_reset_cache_on_source_file_changes: false,
        fast_refresh_source_hash: None,
        disable_memoization_features: false,
        disable_memoization_for_debugging: false,
        fbt_operands: HashSet::new(),
        cache_binding_name: None,
        unique_identifiers: HashSet::new(),
        param_name_overrides: HashMap::new(),
        enable_name_anonymous_functions: cx.options.enable_name_anonymous_functions,
    };

    let result = codegen_reactive_function(cx.builder, cx.allocator, &reactive_fn, options);

    let mut directives = cx.builder.vec();
    let directives_empty = hir_func.directives.is_empty();
    for directive in &hir_func.directives {
        directives.push(
            cx.builder.directive(
                SPAN,
                cx.builder
                    .string_literal(SPAN, cx.builder.atom(directive), None),
                cx.builder.atom(directive),
            ),
        );
    }

    let mut body_stmts = result.body;
    // When the last body statement is an assignment expression `name = ...;`,
    // add a trailing expression statement with just the variable name.
    // This matches upstream Babel codegen which emits the block value expression
    // after the assignment (e.g., `count = count + x; count`).
    if let Some(ast::Statement::ExpressionStatement(last_expr)) = body_stmts.last()
        && let ast::Expression::AssignmentExpression(assign) = &last_expr.expression
        && assign.operator == oxc_syntax::operator::AssignmentOperator::Assign
        && let ast::AssignmentTarget::AssignmentTargetIdentifier(ident) = &assign.left
    {
        let name = ident.name.as_str();
        let trailing = cx.builder.statement_expression(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(name)),
        );
        body_stmts.push(trailing);
    }

    let body = cx
        .builder
        .alloc(cx.builder.function_body(SPAN, directives, body_stmts));

    // Build params from the reactive function's param list.
    let mut param_items = cx.builder.vec();
    let mut rest_param = None;
    for param in &reactive_fn.params {
        let (place, is_spread) = match param {
            Argument::Place(p) => (p, false),
            Argument::Spread(p) => (p, true),
        };
        let param_name = place
            .identifier
            .name
            .as_ref()
            .map(|n| n.value().to_string())
            .unwrap_or_else(|| format!("t{}", place.identifier.id.0));
        let pattern = cx
            .builder
            .binding_pattern_binding_identifier(SPAN, cx.builder.ident(&param_name));
        if is_spread {
            rest_param = Some(cx.builder.alloc_formal_parameter_rest(
                SPAN,
                cx.builder.vec(),
                cx.builder.binding_rest_element(SPAN, pattern),
                NONE,
            ));
        } else {
            param_items.push(cx.builder.plain_formal_parameter(SPAN, pattern));
        }
    }
    let params = cx.builder.formal_parameters(
        SPAN,
        ast::FormalParameterKind::FormalParameter,
        param_items,
        rest_param,
    );

    match expr_type {
        FunctionExpressionType::ArrowFunctionExpression => {
            // Detect single-return-expression bodies and convert to expression arrows.
            let (is_expression, body) = if directives_empty
                && body.statements.len() == 1
                && matches!(body.statements[0], ast::Statement::ReturnStatement(_))
            {
                if let ast::Statement::ReturnStatement(ret) = &body.statements[0] {
                    if let Some(arg) = &ret.argument {
                        let expr = arg.clone_in(cx.allocator);
                        let expr_body = cx.builder.alloc(cx.builder.function_body(
                            SPAN,
                            cx.builder.vec(),
                            cx.builder.vec1(cx.builder.statement_expression(SPAN, expr)),
                        ));
                        (true, expr_body)
                    } else {
                        (false, body)
                    }
                } else {
                    (false, body)
                }
            } else {
                (false, body)
            };
            Some(cx.builder.expression_arrow_function(
                SPAN,
                is_expression,
                hir_func.async_,
                NONE,
                cx.builder.alloc(params),
                NONE,
                body,
            ))
        }
        _ => Some(cx.builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            name.map(|n| cx.builder.binding_identifier(SPAN, cx.builder.atom(n))),
            hir_func.generator,
            hir_func.async_,
            false,
            NONE,
            NONE,
            cx.builder.alloc(params),
            NONE,
            Some(body),
        )),
    }
}

// ---------------------------------------------------------------------------
// Anonymous function naming
// ---------------------------------------------------------------------------

/// Wrap an anonymous function expression with a named object pattern:
/// `{ "name_hint": <expr> }["name_hint"]`
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

// ---------------------------------------------------------------------------
// Hook guard helpers
// ---------------------------------------------------------------------------

const HOOK_GUARD_IDENT: &str = "$dispatcherGuard";
const HOOK_GUARD_ALLOW: u8 = 2;
const HOOK_GUARD_DISALLOW: u8 = 3;

fn is_hook_name_str(name: &str) -> bool {
    name.len() >= 4 && name.starts_with("use") && name.as_bytes()[3].is_ascii_uppercase()
}

/// Check if a resolved callee expression is a hook name (identifier starting with `use` + uppercase).
fn expr_is_hook_name(expr: &ast::Expression<'_>) -> bool {
    match expr {
        ast::Expression::Identifier(ident) => is_hook_name_str(ident.name.as_str()),
        _ => false,
    }
}

/// Check if a method call callee expression has a hook-name property.
/// E.g., `ObjectWithHooks.useIdentity` → checks "useIdentity".
fn method_property_is_hook_name(callee_expr: &ast::Expression<'_>) -> bool {
    match callee_expr {
        ast::Expression::StaticMemberExpression(member) => {
            is_hook_name_str(member.property.name.as_str())
        }
        _ => false,
    }
}

/// Wrap a hook call expression in an IIFE with dispatcher guard:
/// ```js
/// (function() { try { $dispatcherGuard(2); return hookCall(); } finally { $dispatcherGuard(3); } })()
/// ```
fn wrap_hook_guard_iife<'a>(
    cx: &mut CodegenContext<'a>,
    call_expr: ast::Expression<'a>,
) -> ast::Expression<'a> {
    cx.emitted_hook_guards = true;
    cx.needs_function_hook_guard_wrapper = true;

    let guard_allow = cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(HOOK_GUARD_IDENT)),
            NONE,
            cx.builder
                .vec1(ast::Argument::from(cx.builder.expression_numeric_literal(
                    SPAN,
                    HOOK_GUARD_ALLOW as f64,
                    None,
                    NumberBase::Decimal,
                ))),
            false,
        ),
    );

    let return_stmt = cx.builder.statement_return(SPAN, Some(call_expr));

    let guard_disallow = cx.builder.statement_expression(
        SPAN,
        cx.builder.expression_call(
            SPAN,
            cx.builder
                .expression_identifier(SPAN, cx.builder.ident(HOOK_GUARD_IDENT)),
            NONE,
            cx.builder
                .vec1(ast::Argument::from(cx.builder.expression_numeric_literal(
                    SPAN,
                    HOOK_GUARD_DISALLOW as f64,
                    None,
                    NumberBase::Decimal,
                ))),
            false,
        ),
    );

    let try_body = cx.builder.vec_from_iter([guard_allow, return_stmt]);

    let try_stmt = cx.builder.statement_try(
        SPAN,
        cx.builder.alloc_block_statement(SPAN, try_body),
        Option::<oxc_allocator::Box<'_, ast::CatchClause<'_>>>::None,
        Some(
            cx.builder
                .alloc_block_statement(SPAN, cx.builder.vec1(guard_disallow)),
        ),
    );

    let function_expr =
        cx.builder.expression_function(
            SPAN,
            ast::FunctionType::FunctionExpression,
            None,
            false,
            false,
            false,
            NONE,
            NONE,
            cx.builder.alloc(cx.builder.formal_parameters(
                SPAN,
                ast::FormalParameterKind::FormalParameter,
                cx.builder.vec(),
                Option::<oxc_allocator::Box<'_, ast::FormalParameterRest<'_>>>::None,
            )),
            NONE,
            Some(cx.builder.alloc(cx.builder.function_body(
                SPAN,
                cx.builder.vec(),
                cx.builder.vec1(try_stmt),
            ))),
        );

    cx.builder
        .expression_call(SPAN, function_expr, NONE, cx.builder.vec(), false)
}

/// Check if a scope body is trivial (no calls, no allocations, no JSX).
/// Bodies with only loads, stores, declarations, operators, casts, and calls (no JSX/objects/arrays).
fn scope_body_is_trivial_or_calls(instructions: &ReactiveBlock) -> bool {
    instructions.iter().all(|s| match s {
        ReactiveStatement::Instruction(instr) => matches!(
            &instr.value,
            InstructionValue::Primitive { .. }
                | InstructionValue::LoadLocal { .. }
                | InstructionValue::LoadContext { .. }
                | InstructionValue::StoreLocal { .. }
                | InstructionValue::StoreContext { .. }
                | InstructionValue::DeclareLocal { .. }
                | InstructionValue::DeclareContext { .. }
                | InstructionValue::BinaryExpression { .. }
                | InstructionValue::UnaryExpression { .. }
                | InstructionValue::TypeCastExpression { .. }
                | InstructionValue::LoadGlobal { .. }
                | InstructionValue::CallExpression { .. }
                | InstructionValue::MethodCall { .. }
                | InstructionValue::PropertyLoad { .. }
                | InstructionValue::Destructure { .. }
        ),
        _ => false,
    })
}

/// Strictly trivial body: no calls at all, only loads/stores/operators.
fn scope_body_is_strictly_trivial(instructions: &ReactiveBlock) -> bool {
    instructions.iter().all(|s| match s {
        ReactiveStatement::Instruction(instr) => matches!(
            &instr.value,
            InstructionValue::Primitive { .. }
                | InstructionValue::LoadLocal { .. }
                | InstructionValue::LoadContext { .. }
                | InstructionValue::StoreLocal { .. }
                | InstructionValue::StoreContext { .. }
                | InstructionValue::DeclareLocal { .. }
                | InstructionValue::DeclareContext { .. }
                | InstructionValue::BinaryExpression { .. }
                | InstructionValue::UnaryExpression { .. }
                | InstructionValue::TypeCastExpression { .. }
                | InstructionValue::LoadGlobal { .. }
        ),
        _ => false,
    })
}

/// Check whether a HIR function body has a side-effecting unnamed temp whose
/// reference is NOT tracked by the `hir_to_ast` fast path's `collect_instruction_uses`.
/// When detected, callers skip the fast path and use the reactive codegen path which
/// handles temp inlining correctly.
///
/// The `hir_to_ast` fast path decides whether to emit a side-effecting instruction as a
/// standalone expression statement based on whether its temp lvalue appears in `used_temps`.
/// However, `collect_instruction_uses` doesn't handle all instruction types (e.g.,
/// ArrayExpression, ObjectExpression, Destructure, JSX, etc.), so temps consumed only by
/// those instructions are missing from `used_temps`.  This causes the side-effecting
/// expression to be emitted BOTH as a standalone statement AND inlined when the temp is
/// later resolved via `lower_place`.
fn has_destructure_consuming_side_effecting_temp(hir_func: &HIRFunction) -> bool {
    // Collect unnamed temp IDs whose instruction is side-effecting.
    let mut side_effecting_temps: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir_func.body.blocks {
        for instr in &block.instructions {
            if instr.lvalue.identifier.name.is_none()
                && matches!(
                    instr.value,
                    InstructionValue::CallExpression { .. }
                        | InstructionValue::MethodCall { .. }
                        | InstructionValue::NewExpression { .. }
                )
            {
                side_effecting_temps.insert(instr.lvalue.identifier.id);
            }
        }
    }
    if side_effecting_temps.is_empty() {
        return false;
    }

    // Collect the temp uses that the hir_to_ast fast path DOES track (mirroring
    // collect_instruction_uses in hir_to_ast.rs).  Any side-effecting temp NOT
    // in this set but still referenced would be duplicated by the fast path.
    let mut tracked_uses: HashSet<IdentifierId> = HashSet::new();
    for (_, block) in &hir_func.body.blocks {
        for instr in &block.instructions {
            collect_tracked_temp_uses(&instr.value, &mut tracked_uses);
        }
    }

    // If any side-effecting temp is not in tracked_uses, the fast path would
    // emit it as a standalone expression statement AND inline it, causing a
    // duplicate.
    side_effecting_temps
        .iter()
        .any(|id| !tracked_uses.contains(id))
}

/// Mirrors `collect_instruction_uses` from hir_to_ast.rs, recording unnamed
/// temp references for the instruction types that the fast path tracks.
/// Instruction types not listed here fall through to `_ => {}` in the fast
/// path and would NOT prevent a side-effecting temp from being duplicated.
fn collect_tracked_temp_uses(value: &InstructionValue, used: &mut HashSet<IdentifierId>) {
    fn record(place: &Place, used: &mut HashSet<IdentifierId>) {
        if place.identifier.name.is_none() {
            used.insert(place.identifier.id);
        }
    }
    fn record_args(args: &[Argument], used: &mut HashSet<IdentifierId>) {
        for arg in args {
            match arg {
                Argument::Place(p) | Argument::Spread(p) => record(p, used),
            }
        }
    }
    match value {
        InstructionValue::LoadLocal { place, .. } | InstructionValue::LoadContext { place, .. } => {
            record(place, used)
        }
        InstructionValue::StoreLocal { value: v, .. }
        | InstructionValue::StoreContext { value: v, .. } => record(v, used),
        InstructionValue::BinaryExpression { left, right, .. } => {
            record(left, used);
            record(right, used);
        }
        InstructionValue::UnaryExpression { value: v, .. }
        | InstructionValue::TypeCastExpression { value: v, .. } => record(v, used),
        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            record(callee, used);
            record_args(args, used);
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            record(receiver, used);
            record(property, used);
            record_args(args, used);
        }
        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyDelete { object, .. } => record(object, used),
        InstructionValue::PropertyStore {
            object, value: v, ..
        } => {
            record(object, used);
            record(v, used);
        }
        InstructionValue::ComputedLoad {
            object, property, ..
        }
        | InstructionValue::ComputedDelete {
            object, property, ..
        } => {
            record(object, used);
            record(property, used);
        }
        InstructionValue::ComputedStore {
            object,
            property,
            value: v,
            ..
        } => {
            record(object, used);
            record(property, used);
            record(v, used);
        }
        InstructionValue::StoreGlobal { value: v, .. } => record(v, used),
        InstructionValue::LogicalExpression { left, right, .. } => {
            record(left, used);
            record(right, used);
        }
        InstructionValue::Ternary {
            test,
            consequent,
            alternate,
            ..
        } => {
            record(test, used);
            record(consequent, used);
            record(alternate, used);
        }
        InstructionValue::PrefixUpdate { lvalue, .. }
        | InstructionValue::PostfixUpdate { lvalue, .. } => record(lvalue, used),
        _ => {}
    }
}

/// Parse a type annotation string (e.g. `"Foo"`, `"Foo<Bar>"`, `"Foo | Bar"`) into an OXC
/// `TSType` AST node.  Falls back to a simple `TSTypeReference` with an identifier when
/// parsing fails (e.g. Flow-specific syntax OXC can't handle).
fn parse_ts_type<'a>(
    allocator: &'a Allocator,
    builder: &AstBuilder<'a>,
    type_source: &str,
) -> ast::TSType<'a> {
    let wrapper = format!("const __t: {type_source} = null;");
    let parsed = Parser::new(
        allocator,
        allocator.alloc_str(&wrapper),
        oxc_span::SourceType::ts().with_jsx(true),
    )
    .parse();
    if !parsed.panicked
        && parsed.errors.is_empty()
        && let Some(ast::Statement::VariableDeclaration(decl)) =
            parsed.program.body.into_iter().next()
        && let Some(annotation) = decl
            .unbox()
            .declarations
            .into_iter()
            .next()
            .and_then(|d| d.type_annotation)
    {
        return annotation.unbox().type_annotation;
    }
    // Fallback: bare identifier reference type.
    builder.ts_type_type_reference(
        SPAN,
        builder.ts_type_name_identifier_reference(SPAN, builder.ident(type_source)),
        NONE,
    )
}

/// Check if a name is a codegen temp name (`tN` where N is numeric).
fn is_codegen_temp(name: &str) -> bool {
    name.starts_with('t') && name.len() >= 2 && name[1..].chars().all(|c| c.is_ascii_digit())
}

/// Record a named identifier's declaration_id → name mapping.
fn remember_ident(names: &mut HashMap<DeclarationId, String>, ident: &Identifier) {
    if let Some(name) = ident.name.as_ref() {
        names
            .entry(ident.declaration_id)
            .or_insert_with(|| name.value().to_string());
    }
}

/// Scan Place identifiers in an instruction value for preferred names.
fn scan_instruction_value_identifiers(
    value: &InstructionValue,
    names: &mut HashMap<DeclarationId, String>,
) {
    match value {
        InstructionValue::LoadLocal { place, .. }
        | InstructionValue::LoadContext { place, .. }
        | InstructionValue::GetIterator {
            collection: place, ..
        }
        | InstructionValue::IteratorNext {
            collection: place, ..
        }
        | InstructionValue::NextPropertyOf { value: place, .. }
        | InstructionValue::Await { value: place, .. }
        | InstructionValue::TypeCastExpression { value: place, .. } => {
            remember_ident(names, &place.identifier);
        }
        InstructionValue::StoreLocal {
            lvalue, value: val, ..
        }
        | InstructionValue::StoreContext {
            lvalue, value: val, ..
        } => {
            remember_ident(names, &lvalue.place.identifier);
            remember_ident(names, &val.identifier);
        }
        InstructionValue::Destructure {
            value: val,
            lvalue: pat,
            ..
        } => {
            remember_ident(names, &val.identifier);
            scan_pattern_identifiers(pat, names);
        }
        InstructionValue::BinaryExpression { left, right, .. }
        | InstructionValue::LogicalExpression { left, right, .. } => {
            remember_ident(names, &left.identifier);
            remember_ident(names, &right.identifier);
        }
        InstructionValue::UnaryExpression { value: val, .. }
        | InstructionValue::PrefixUpdate { value: val, .. }
        | InstructionValue::PostfixUpdate { value: val, .. } => {
            remember_ident(names, &val.identifier);
        }
        InstructionValue::CallExpression { callee, args, .. }
        | InstructionValue::NewExpression { callee, args, .. } => {
            remember_ident(names, &callee.identifier);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        remember_ident(names, &p.identifier);
                    }
                }
            }
        }
        InstructionValue::MethodCall {
            receiver,
            property,
            args,
            ..
        } => {
            remember_ident(names, &receiver.identifier);
            remember_ident(names, &property.identifier);
            for arg in args {
                match arg {
                    Argument::Place(p) | Argument::Spread(p) => {
                        remember_ident(names, &p.identifier);
                    }
                }
            }
        }
        InstructionValue::PropertyLoad { object, .. }
        | InstructionValue::PropertyStore { object, .. }
        | InstructionValue::PropertyDelete { object, .. }
        | InstructionValue::ComputedLoad { object, .. }
        | InstructionValue::ComputedStore { object, .. }
        | InstructionValue::ComputedDelete { object, .. } => {
            remember_ident(names, &object.identifier);
        }
        _ => {}
    }
}

fn scan_pattern_identifiers(pattern: &LValuePattern, names: &mut HashMap<DeclarationId, String>) {
    match &pattern.pattern {
        Pattern::Array(array_pat) => {
            for item in &array_pat.items {
                match item {
                    ArrayElement::Place(place) | ArrayElement::Spread(place) => {
                        remember_ident(names, &place.identifier);
                    }
                    ArrayElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj_pat) => {
            for prop in &obj_pat.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => {
                        remember_ident(names, &p.place.identifier);
                    }
                    ObjectPropertyOrSpread::Spread(place) => {
                        remember_ident(names, &place.identifier);
                    }
                }
            }
        }
    }
}

/// Collect preferred declaration names from a reactive block.
/// Maps DeclarationId → first named reference found in the body.
/// Used to resolve unnamed param identifiers to their preferred display name.
fn collect_preferred_decl_names(block: &ReactiveBlock) -> HashMap<DeclarationId, String> {
    let mut names = HashMap::new();
    collect_preferred_decl_names_impl(block, &mut names);
    names
}

fn collect_preferred_decl_names_impl(
    block: &ReactiveBlock,
    names: &mut HashMap<DeclarationId, String>,
) {
    for stmt in block.iter() {
        match stmt {
            ReactiveStatement::Instruction(instr) => {
                if let Some(lvalue) = &instr.lvalue
                    && let Some(name) = lvalue.identifier.name.as_ref()
                {
                    names
                        .entry(lvalue.identifier.declaration_id)
                        .or_insert_with(|| name.value().to_string());
                }
                // Also scan places in instruction values for named identifiers.
                scan_instruction_value_identifiers(&instr.value, names);
            }
            ReactiveStatement::Scope(scope) => {
                // Scan scope declarations for named identifiers.
                for decl in scope.scope.declarations.values() {
                    if let Some(name) = decl.identifier.name.as_ref() {
                        names
                            .entry(decl.identifier.declaration_id)
                            .or_insert_with(|| name.value().to_string());
                    }
                }
                // Scan scope reassignments.
                for reassign in &scope.scope.reassignments {
                    if let Some(name) = reassign.name.as_ref() {
                        names
                            .entry(reassign.declaration_id)
                            .or_insert_with(|| name.value().to_string());
                    }
                }
                collect_preferred_decl_names_impl(&scope.instructions, names);
            }
            ReactiveStatement::PrunedScope(scope) => {
                for decl in scope.scope.declarations.values() {
                    if let Some(name) = decl.identifier.name.as_ref() {
                        names
                            .entry(decl.identifier.declaration_id)
                            .or_insert_with(|| name.value().to_string());
                    }
                }
                for reassign in &scope.scope.reassignments {
                    if let Some(name) = reassign.name.as_ref() {
                        names
                            .entry(reassign.declaration_id)
                            .or_insert_with(|| name.value().to_string());
                    }
                }
                collect_preferred_decl_names_impl(&scope.instructions, names);
            }
            ReactiveStatement::Terminal(term_stmt) => {
                collect_preferred_decl_names_in_terminal(&term_stmt.terminal, names);
            }
        }
    }
}

fn collect_preferred_decl_names_in_terminal(
    terminal: &ReactiveTerminal,
    names: &mut HashMap<DeclarationId, String>,
) {
    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            collect_preferred_decl_names_impl(consequent, names);
            if let Some(alt) = alternate {
                collect_preferred_decl_names_impl(alt, names);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &case.block {
                    collect_preferred_decl_names_impl(block, names);
                }
            }
        }
        ReactiveTerminal::For {
            init,
            update,
            loop_block,
            ..
        } => {
            collect_preferred_decl_names_impl(init, names);
            if let Some(update) = update {
                collect_preferred_decl_names_impl(update, names);
            }
            collect_preferred_decl_names_impl(loop_block, names);
        }
        ReactiveTerminal::While { loop_block, .. }
        | ReactiveTerminal::DoWhile { loop_block, .. }
        | ReactiveTerminal::ForOf { loop_block, .. }
        | ReactiveTerminal::ForIn { loop_block, .. }
        | ReactiveTerminal::Label {
            block: loop_block, ..
        } => {
            collect_preferred_decl_names_impl(loop_block, names);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            collect_preferred_decl_names_impl(block, names);
            collect_preferred_decl_names_impl(handler, names);
        }
        _ => {}
    }
}
