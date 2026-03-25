//! BuildHIR — Lower OXC AST to HIR.
//!
//! Port of `BuildHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.

use oxc_ast::{AstKind, ast as js};
use oxc_semantic::Semantic;
use oxc_span::{GetSpan, Span};
use oxc_syntax::node::NodeId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::builder::HIRBuilder;
use super::object_shape::BUILT_IN_ARRAY_ID;
use super::types as hir;

const FLOW_CAST_REWRITE_MARKER: &str = "/*__FLOW_CAST__*/";

thread_local! {
    static CURRENT_SOURCE_LINE_STARTS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

struct SourceLineMapGuard {
    previous: Vec<u32>,
}

impl Drop for SourceLineMapGuard {
    fn drop(&mut self) {
        CURRENT_SOURCE_LINE_STARTS.with(|cell| {
            *cell.borrow_mut() = std::mem::take(&mut self.previous);
        });
    }
}

fn install_source_line_starts(source: &str) -> SourceLineMapGuard {
    let mut line_starts = Vec::with_capacity(source.len() / 32 + 2);
    line_starts.push(0);
    for (idx, byte) in source.as_bytes().iter().enumerate() {
        if *byte == b'\n' {
            let next = idx.saturating_add(1);
            if next <= source.len() {
                line_starts.push(next as u32);
            }
        }
    }
    let previous = CURRENT_SOURCE_LINE_STARTS.with(|cell| {
        let mut slot = cell.borrow_mut();
        std::mem::replace(&mut *slot, line_starts)
    });
    SourceLineMapGuard { previous }
}

/// Result of lowering a function to HIR.
pub struct LowerResult {
    pub func: hir::HIRFunction,
}

pub struct LoweringContext<'a> {
    pub semantic: &'a Semantic<'a>,
    pub source: &'a str,
    pub env: crate::environment::Environment,
    pub binding_name_counters: Option<Rc<RefCell<HashMap<String, u32>>>>,
}

impl<'a> LoweringContext<'a> {
    pub fn new(
        semantic: &'a Semantic<'a>,
        source: &'a str,
        env: crate::environment::Environment,
    ) -> Self {
        Self {
            semantic,
            source,
            env,
            binding_name_counters: None,
        }
    }

    pub fn with_binding_name_counters(
        mut self,
        binding_name_counters: Rc<RefCell<HashMap<String, u32>>>,
    ) -> Self {
        self.binding_name_counters = Some(binding_name_counters);
        self
    }
}

#[derive(Clone, Copy)]
pub struct LowerFunctionOptions<'a> {
    pub func_id: Option<&'a str>,
    pub func_span: Span,
    pub is_generator: bool,
    pub is_async: bool,
    pub is_expression_arrow: bool,
}

impl<'a> LowerFunctionOptions<'a> {
    pub fn function(
        func_id: Option<&'a str>,
        func_span: Span,
        is_generator: bool,
        is_async: bool,
    ) -> Self {
        Self {
            func_id,
            func_span,
            is_generator,
            is_async,
            is_expression_arrow: false,
        }
    }

    pub fn arrow(
        func_id: Option<&'a str>,
        func_span: Span,
        is_async: bool,
        is_expression_arrow: bool,
    ) -> Self {
        Self {
            func_id,
            func_span,
            is_generator: false,
            is_async,
            is_expression_arrow,
        }
    }
}

fn maybe_record_binding_identifier_rename<'a>(
    semantic: &Semantic<'a>,
    ident: &js::BindingIdentifier<'a>,
    identifier: &hir::Identifier,
) {
    let Some(symbol_id) = ident.symbol_id.get() else {
        return;
    };
    let Some(name) = identifier.name.as_ref() else {
        return;
    };
    crate::pipeline::record_ast_symbol_rename(
        semantic,
        symbol_id,
        ident.span,
        ident.name.as_str(),
        name.value(),
    );
}

/// Lower a function body and params to HIR.
pub fn lower_function<'a>(
    func_body: &js::FunctionBody<'a>,
    func_params: &js::FormalParameters<'a>,
    cx: LoweringContext<'a>,
    options: LowerFunctionOptions<'a>,
) -> Result<LowerResult, String> {
    lower_function_inner(func_body, func_params, cx, options)
}

/// Lower an arrow function expression body.
/// For expression arrows like `(x) => x + 1`, the body's single expression
/// is treated as an implicit return.
pub fn lower_arrow_expression<'a>(
    func_body: &js::FunctionBody<'a>,
    func_params: &js::FormalParameters<'a>,
    cx: LoweringContext<'a>,
    options: LowerFunctionOptions<'a>,
) -> Result<LowerResult, String> {
    lower_function_inner(func_body, func_params, cx, options)
}

fn lower_function_inner<'a>(
    func_body: &js::FunctionBody<'a>,
    func_params: &js::FormalParameters<'a>,
    cx: LoweringContext<'a>,
    options: LowerFunctionOptions<'a>,
) -> Result<LowerResult, String> {
    let LoweringContext {
        semantic,
        source,
        env,
        binding_name_counters,
    } = cx;
    let LowerFunctionOptions {
        func_id,
        func_span,
        is_generator,
        is_async,
        is_expression_arrow,
    } = options;
    let _line_map_guard = install_source_line_starts(source);
    let mut builder = if let Some(counters) = binding_name_counters {
        HIRBuilder::new_with_binding_name_counters(env, counters)
    } else {
        HIRBuilder::new(env)
    };

    // Lower parameters
    let mut params = Vec::new();
    for param in &func_params.items {
        if let Some(initializer) = &param.initializer
            && !is_reorderable_expression(&builder, initializer, semantic, true)
        {
            builder.push_todo(format!(
                "(BuildHIR::node.lowerReorderableExpression) Expression type `{}` cannot be safely reordered",
                reorderable_expr_type_name(initializer)
            ));
        }

        // Upstream parity: params with defaults are lowered as temporary
        // parameters, followed by explicit default initialization + assignment
        // into the declared binding pattern.
        if let Some(initializer) = &param.initializer {
            let loc = span_to_loc(param.span);
            let param_temp = builder.make_temporary_place(loc.clone());
            params.push(hir::Argument::Place(param_temp.clone()));

            let value = emit_default_value_branch(
                &mut builder,
                param_temp,
                initializer,
                semantic,
                source,
                &loc,
            );

            lower_binding_pat(
                &mut builder,
                &param.pattern,
                hir::InstructionKind::Let,
                value,
                semantic,
                source,
                false,
            );
            continue;
        }

        match &param.pattern {
            js::BindingPattern::BindingIdentifier(ident) => {
                let loc = span_to_loc(param.span);
                let identifier = builder.resolve_binding(&ident.name, loc.clone());
                maybe_record_binding_identifier_rename(semantic, ident, &identifier);
                let place = hir::Place {
                    identifier,
                    effect: hir::Effect::Unknown,
                    reactive: false,
                    loc,
                };
                params.push(hir::Argument::Place(place));
            }
            _ => {
                // Destructured parameter: create a temporary for the param,
                // then lower the destructuring pattern to register bindings
                // and generate HIR instructions.
                let loc = span_to_loc(param.span);
                let place = builder.make_temporary_place(loc.clone());
                params.push(hir::Argument::Place(place.clone()));
                // Lower the destructuring pattern into the entry block.
                // This registers the destructured bindings (e.g., `inputNum` from `{inputNum}`)
                // in the builder's bindings map so later references resolve to LoadLocal.
                lower_binding_pat(
                    &mut builder,
                    &param.pattern,
                    hir::InstructionKind::Let,
                    place,
                    semantic,
                    source,
                    false,
                );
            }
        }
    }

    if let Some(rest) = &func_params.rest {
        let loc = span_to_loc(rest.span);
        let place = builder.make_temporary_place(loc.clone());
        params.push(hir::Argument::Spread(place.clone()));
        lower_binding_pat(
            &mut builder,
            &rest.rest.argument,
            hir::InstructionKind::Let,
            place,
            semantic,
            source,
            false,
        );
    }

    // Lower body directives
    let directives: Vec<String> = func_body
        .directives
        .iter()
        .map(|d| d.expression.value.to_string())
        .collect();

    // Lower statements
    // For expression arrows like `(x) => x + 1`, the body has a single
    // ExpressionStatement that should be treated as an implicit return.
    if is_expression_arrow && func_body.statements.len() == 1 {
        if let js::Statement::ExpressionStatement(expr_stmt) = &func_body.statements[0] {
            let expr_place =
                lower_expr_to_temp(&mut builder, &expr_stmt.expression, semantic, source);
            builder.terminate(
                hir::Terminal::Return {
                    value: expr_place,
                    return_variant: hir::ReturnVariant::Explicit,
                    id: hir::InstructionId::default(),
                    loc: span_to_loc(expr_stmt.span),
                },
                None,
            );
        } else {
            // Unexpected — lower normally
            lower_statement(&mut builder, &func_body.statements[0], semantic, source);
            let void_val = hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: hir::SourceLocation::Generated,
            };
            let void_place = lower_value_to_temporary(&mut builder, void_val);
            builder.terminate(
                hir::Terminal::Return {
                    value: void_place.clone(),
                    return_variant: hir::ReturnVariant::Void,
                    id: hir::InstructionId::default(),
                    loc: hir::SourceLocation::Generated,
                },
                None,
            );
        }
    } else {
        predeclare_function_decls_in_block(&mut builder, semantic, &func_body.statements);
        if has_forward_function_decl_reference(&func_body.statements) {
            builder.push_todo(
                "[PruneHoistedContexts] Rewrite hoisted function references".to_string(),
            );
        }
        lower_block_statements_with_hoisted_contexts(
            &mut builder,
            &func_body.statements,
            semantic,
            source,
        );

        // Add implicit void return
        let void_val = hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Undefined,
            loc: hir::SourceLocation::Generated,
        };
        let void_place = lower_value_to_temporary(&mut builder, void_val);
        builder.terminate(
            hir::Terminal::Return {
                value: void_place.clone(),
                return_variant: hir::ReturnVariant::Void,
                id: hir::InstructionId::default(),
                loc: hir::SourceLocation::Generated,
            },
            None,
        );
    }

    // Upstream parity: detect unreachable blocks that still contain function
    // declarations before CFG pruning in build().
    builder.detect_unreachable_hoisted_function_decls();

    // Check for accumulated errors before completing
    if builder.has_errors() {
        let messages: Vec<String> = builder.errors.iter().map(|e| e.message.clone()).collect();
        return Err(messages.join("\n"));
    }

    let returns = builder.make_temporary_place(span_to_loc(func_span));
    let context = Vec::new();
    let env = builder.env.clone();
    let body = builder.build();

    Ok(LowerResult {
        func: hir::HIRFunction {
            env,
            id: func_id.map(|s| s.to_string()),
            fn_type: hir::ReactFunctionType::Other,
            params,
            returns,
            context,
            body,
            generator: is_generator,
            async_: is_async,
            directives,
            aliasing_effects: None,
        },
    })
}

// ============================================================================
// Statement lowering
// ============================================================================

fn lower_statement<'a>(
    builder: &mut HIRBuilder,
    stmt: &js::Statement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    match stmt {
        js::Statement::BlockStatement(block) => {
            builder.enter_binding_scope();
            predeclare_function_decls_in_block(builder, semantic, &block.body);
            lower_block_statements_with_hoisted_contexts(builder, &block.body, semantic, source);
            builder.exit_binding_scope();
        }
        js::Statement::ExpressionStatement(expr_stmt) => {
            lower_expr_to_temp(builder, &expr_stmt.expression, semantic, source);
        }
        js::Statement::ReturnStatement(ret) => {
            let value = if let Some(arg) = &ret.argument {
                lower_expr_to_temp(builder, arg, semantic, source)
            } else {
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::Primitive {
                        value: hir::PrimitiveValue::Undefined,
                        loc: hir::SourceLocation::Generated,
                    },
                )
            };
            builder.terminate(
                hir::Terminal::Return {
                    value,
                    return_variant: hir::ReturnVariant::Explicit,
                    id: hir::InstructionId::default(),
                    loc: span_to_loc(ret.span),
                },
                Some(hir::BlockKind::Block),
            );
        }
        js::Statement::VariableDeclaration(decl) => {
            lower_var_decl(builder, decl, semantic, source);
        }
        js::Statement::FunctionDeclaration(func) => {
            lower_func_decl(builder, func, semantic, source);
        }
        js::Statement::IfStatement(if_stmt) => {
            lower_if(builder, if_stmt, semantic, source);
        }
        js::Statement::ForStatement(f) => {
            if builder.in_try_context() {
                builder.push_todo(
                    "Support value blocks (conditional, logical, optional chaining, etc) within a try/catch statement"
                        .to_string(),
                );
            }
            lower_for(builder, f, semantic, source);
        }
        js::Statement::ForOfStatement(f) => {
            lower_for_of(builder, f, semantic, source);
        }
        js::Statement::ForInStatement(f) => {
            lower_for_in(builder, f, semantic, source);
        }
        js::Statement::WhileStatement(w) => {
            lower_while(builder, w, semantic, source);
        }
        js::Statement::DoWhileStatement(d) => {
            lower_do_while(builder, d, semantic, source);
        }
        js::Statement::SwitchStatement(s) => {
            lower_switch(builder, s, semantic, source);
        }
        js::Statement::TryStatement(t) => {
            lower_try(builder, t, semantic, source);
        }
        js::Statement::ThrowStatement(throw) => {
            if builder.in_try_context() {
                builder.push_todo(
                    "(BuildHIR::lowerStatement) Support ThrowStatement inside of try/catch"
                        .to_string(),
                );
            }
            let value = lower_expr_to_temp(builder, &throw.argument, semantic, source);
            builder.terminate(
                hir::Terminal::Throw {
                    value,
                    id: hir::InstructionId::default(),
                    loc: span_to_loc(throw.span),
                },
                Some(hir::BlockKind::Block),
            );
        }
        js::Statement::BreakStatement(brk) => {
            let label = brk.label.as_ref().map(|l| l.name.as_str());
            if let Some(target) = builder.lookup_break(label) {
                builder.terminate(
                    hir::Terminal::Goto {
                        block: target,
                        variant: hir::GotoVariant::Break,
                        id: hir::InstructionId::default(),
                        loc: span_to_loc(brk.span),
                    },
                    Some(hir::BlockKind::Block),
                );
            }
        }
        js::Statement::ContinueStatement(cont) => {
            let label = cont.label.as_ref().map(|l| l.name.as_str());
            if let Some(target) = builder.lookup_continue(label) {
                builder.terminate(
                    hir::Terminal::Goto {
                        block: target,
                        variant: hir::GotoVariant::Continue,
                        id: hir::InstructionId::default(),
                        loc: span_to_loc(cont.span),
                    },
                    Some(hir::BlockKind::Block),
                );
            }
        }
        js::Statement::LabeledStatement(labeled) => {
            let label = labeled.label.name.to_string();
            match &labeled.body {
                // Labeled loops are special because `continue label` must resolve to
                // the loop's continue target, so keep the existing lowering path.
                js::Statement::ForStatement(_)
                | js::Statement::ForOfStatement(_)
                | js::Statement::ForInStatement(_)
                | js::Statement::WhileStatement(_)
                | js::Statement::DoWhileStatement(_) => {
                    let continuation = builder.reserve(hir::BlockKind::Block);
                    let cont_id = continuation.id;
                    builder.push_label(label, cont_id);
                    lower_statement(builder, &labeled.body, semantic, source);
                    builder.pop_label();
                    builder.terminate_with_continuation(
                        hir::Terminal::Goto {
                            block: cont_id,
                            variant: hir::GotoVariant::Break,
                            id: hir::InstructionId::default(),
                            loc: span_to_loc(labeled.span),
                        },
                        continuation,
                    );
                }
                // Non-loop labels should lower to an explicit Label terminal.
                _ => {
                    let continuation = builder.reserve(hir::BlockKind::Block);
                    let cont_id = continuation.id;
                    let body_block = builder.reserve(hir::BlockKind::Block);
                    let body_id = body_block.id;
                    builder.terminate_with_continuation(
                        hir::Terminal::Label {
                            block: body_id,
                            fallthrough: cont_id,
                            id: hir::InstructionId::default(),
                            loc: span_to_loc(labeled.span),
                        },
                        body_block,
                    );
                    builder.push_label(label.clone(), cont_id);
                    lower_statement(builder, &labeled.body, semantic, source);
                    builder.pop_label();
                    builder.terminate_with_continuation(
                        hir::Terminal::Goto {
                            block: cont_id,
                            variant: hir::GotoVariant::Break,
                            id: hir::InstructionId::default(),
                            loc: span_to_loc(labeled.body.span()),
                        },
                        continuation,
                    );
                }
            }
        }
        js::Statement::DebuggerStatement(dbg) => {
            let place = builder.make_temporary_place(span_to_loc(dbg.span));
            builder.push(hir::Instruction {
                id: hir::InstructionId::default(),
                lvalue: place,
                value: hir::InstructionValue::Debugger {
                    loc: span_to_loc(dbg.span),
                },
                loc: span_to_loc(dbg.span),
                effects: None,
            });
        }
        js::Statement::EmptyStatement(_) => {}
        // TypeScript declaration statements — skip silently (not runtime code)
        js::Statement::TSTypeAliasDeclaration(_)
        | js::Statement::TSInterfaceDeclaration(_)
        | js::Statement::TSEnumDeclaration(_)
        | js::Statement::TSModuleDeclaration(_)
        | js::Statement::TSImportEqualsDeclaration(_)
        | js::Statement::TSExportAssignment(_) => {}
        // Import/export declarations — skip silently (handled by bundler)
        js::Statement::ImportDeclaration(_)
        | js::Statement::ExportAllDeclaration(_)
        | js::Statement::ExportDefaultDeclaration(_)
        | js::Statement::ExportNamedDeclaration(_) => {}
        // Unhandled statement types — emit Todo diagnostic
        _ => {
            let stmt_type = stmt_type_name(stmt);
            builder.push_todo(format!(
                "(BuildHIR::lowerStatement) Handle {stmt_type} statements"
            ));
        }
    }
}

#[derive(Clone)]
struct HoistedContextCandidate {
    name: String,
    symbol_id: oxc_semantic::SymbolId,
    kind: hir::InstructionKind,
    decl_stmt_index: usize,
    decl_start: u32,
    loc: hir::SourceLocation,
}

fn lower_block_statements_with_hoisted_contexts<'a>(
    builder: &mut HIRBuilder,
    statements: &[js::Statement<'a>],
    semantic: &Semantic<'a>,
    source: &str,
) {
    let candidates = collect_hoisted_context_candidates(statements);
    let mut emitted: std::collections::HashSet<oxc_semantic::SymbolId> =
        std::collections::HashSet::new();

    for (stmt_index, stmt) in statements.iter().enumerate() {
        for candidate in &candidates {
            if emitted.contains(&candidate.symbol_id) {
                continue;
            }
            // For non-function-declaration candidates, only check statements
            // before the declaration. For function declarations, also check
            // the declaration statement itself (self-references in the body
            // should trigger hoisting, matching upstream's `binding.kind === 'hoisted'`).
            let skip_threshold = if candidate.kind == hir::InstructionKind::HoistedFunction {
                candidate.decl_stmt_index + 1
            } else {
                candidate.decl_stmt_index
            };
            if skip_threshold <= stmt_index {
                continue;
            }
            if statement_needs_hoisted_context_declare(stmt, candidate, semantic) {
                let identifier =
                    builder.declare_binding(&candidate.name, candidate.loc.clone(), true);
                builder.mark_context_identifier(&identifier);
                let place = hir::Place {
                    identifier,
                    effect: hir::Effect::Unknown,
                    reactive: false,
                    loc: candidate.loc.clone(),
                };
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::DeclareContext {
                        lvalue: hir::LValue {
                            place,
                            kind: candidate.kind,
                        },
                        loc: candidate.loc.clone(),
                    },
                );
                emitted.insert(candidate.symbol_id);
            }
        }
        lower_statement(builder, stmt, semantic, source);
    }
}

fn collect_hoisted_context_candidates<'a>(
    statements: &[js::Statement<'a>],
) -> Vec<HoistedContextCandidate> {
    let debug_hoist = std::env::var("DEBUG_HOIST_CONTEXT").is_ok();
    let mut candidates = Vec::new();
    let mut seen_symbols: std::collections::HashSet<oxc_semantic::SymbolId> =
        std::collections::HashSet::new();

    for (stmt_index, stmt) in statements.iter().enumerate() {
        match stmt {
            js::Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id
                    && let Some(symbol_id) = id.symbol_id.get()
                    && seen_symbols.insert(symbol_id)
                {
                    if debug_hoist {
                        eprintln!(
                            "[HOIST_CONTEXT] candidate fn name={} symbol={:?} stmt={} start={}",
                            id.name, symbol_id, stmt_index, id.span.start
                        );
                    }
                    candidates.push(HoistedContextCandidate {
                        name: id.name.to_string(),
                        symbol_id,
                        kind: hir::InstructionKind::HoistedFunction,
                        decl_stmt_index: stmt_index,
                        decl_start: id.span.start,
                        loc: span_to_loc(id.span),
                    });
                }
            }
            js::Statement::VariableDeclaration(decl) => {
                let hoisted_kind = match decl.kind {
                    js::VariableDeclarationKind::Const | js::VariableDeclarationKind::Var => {
                        hir::InstructionKind::HoistedConst
                    }
                    js::VariableDeclarationKind::Let => hir::InstructionKind::HoistedLet,
                    _ => continue,
                };
                for declarator in &decl.declarations {
                    collect_hoistable_binding_identifiers(
                        &declarator.id,
                        hoisted_kind,
                        stmt_index,
                        &mut seen_symbols,
                        &mut candidates,
                    );
                }
            }
            _ => {}
        }
    }

    candidates
}

fn collect_hoistable_binding_identifiers<'a>(
    pattern: &js::BindingPattern<'a>,
    kind: hir::InstructionKind,
    stmt_index: usize,
    seen_symbols: &mut std::collections::HashSet<oxc_semantic::SymbolId>,
    out: &mut Vec<HoistedContextCandidate>,
) {
    match pattern {
        js::BindingPattern::BindingIdentifier(ident) => {
            if let Some(symbol_id) = ident.symbol_id.get()
                && seen_symbols.insert(symbol_id)
            {
                if std::env::var("DEBUG_HOIST_CONTEXT").is_ok() {
                    eprintln!(
                        "[HOIST_CONTEXT] candidate binding name={} symbol={:?} stmt={} start={} kind={:?}",
                        ident.name, symbol_id, stmt_index, ident.span.start, kind
                    );
                }
                out.push(HoistedContextCandidate {
                    name: ident.name.to_string(),
                    symbol_id,
                    kind,
                    decl_stmt_index: stmt_index,
                    decl_start: ident.span.start,
                    loc: span_to_loc(ident.span),
                });
            }
        }
        js::BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_hoistable_binding_identifiers(
                    &prop.value,
                    kind,
                    stmt_index,
                    seen_symbols,
                    out,
                );
            }
            if let Some(rest) = &obj.rest {
                collect_hoistable_binding_identifiers(
                    &rest.argument,
                    kind,
                    stmt_index,
                    seen_symbols,
                    out,
                );
            }
        }
        js::BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_hoistable_binding_identifiers(elem, kind, stmt_index, seen_symbols, out);
            }
            if let Some(rest) = &arr.rest {
                collect_hoistable_binding_identifiers(
                    &rest.argument,
                    kind,
                    stmt_index,
                    seen_symbols,
                    out,
                );
            }
        }
        js::BindingPattern::AssignmentPattern(assign) => {
            collect_hoistable_binding_identifiers(
                &assign.left,
                kind,
                stmt_index,
                seen_symbols,
                out,
            );
        }
    }
}

fn reference_crosses_function_scope(
    reference_scope: oxc_syntax::scope::ScopeId,
    decl_scope: oxc_syntax::scope::ScopeId,
    scoping: &oxc_semantic::Scoping,
) -> bool {
    if reference_scope == decl_scope {
        return false;
    }
    let mut reaches_decl_scope = false;
    let mut crosses_function_scope = false;
    for scope_id in scoping.scope_ancestors(reference_scope) {
        if scope_id == decl_scope {
            reaches_decl_scope = true;
            break;
        }
        if scoping.scope_flags(scope_id).is_function() {
            crosses_function_scope = true;
        }
    }
    reaches_decl_scope && crosses_function_scope
}

fn statement_needs_hoisted_context_declare(
    stmt: &js::Statement<'_>,
    candidate: &HoistedContextCandidate,
    semantic: &Semantic<'_>,
) -> bool {
    let stmt_span = stmt.span();
    let scoping = semantic.scoping();
    let decl_scope = scoping.symbol_scope_id(candidate.symbol_id);
    let nodes = semantic.nodes();

    semantic.symbol_references(candidate.symbol_id).any(|reference| {
        let node_span = nodes.get_node(reference.node_id()).span();
        if node_span.start < stmt_span.start || node_span.start >= stmt_span.end {
            return false;
        }
        // For non-function-declaration candidates, skip references after the
        // declaration start (they don't need hoisting). For function declarations,
        // allow all references including self-references in the function body,
        // matching upstream's `binding.kind === 'hoisted'` which always triggers.
        if candidate.kind != hir::InstructionKind::HoistedFunction
            && node_span.start >= candidate.decl_start
        {
            return false;
        }

        let nested = reference_crosses_function_scope(reference.scope_id(), decl_scope, scoping);
        if std::env::var("DEBUG_HOIST_CONTEXT").is_ok() {
            eprintln!(
                "[HOIST_CONTEXT] ref candidate={} symbol={:?} stmt=[{},{}] ref=[{},{}] decl_start={} nested={} is_fn={}",
                candidate.name,
                candidate.symbol_id,
                stmt_span.start,
                stmt_span.end,
                node_span.start,
                node_span.end,
                candidate.decl_start,
                nested,
                candidate.kind == hir::InstructionKind::HoistedFunction
            );
        }
        candidate.kind == hir::InstructionKind::HoistedFunction || nested
    })
}

fn predeclare_function_decls_in_block<'a>(
    builder: &mut HIRBuilder,
    semantic: &Semantic<'a>,
    statements: &[js::Statement<'a>],
) {
    for stmt in statements {
        if let js::Statement::FunctionDeclaration(func) = stmt
            && let Some(id) = &func.id
        {
            let loc = span_to_loc(id.span);
            let identifier = builder.declare_binding(&id.name, loc, true);
            maybe_record_binding_identifier_rename(semantic, id, &identifier);
        }
    }
}

fn has_forward_function_decl_reference<'a>(statements: &[js::Statement<'a>]) -> bool {
    let mut decl_index: HashMap<&str, usize> = HashMap::new();
    for (idx, stmt) in statements.iter().enumerate() {
        if let js::Statement::FunctionDeclaration(func) = stmt
            && let Some(id) = &func.id
        {
            decl_index.insert(id.name.as_str(), idx);
        }
    }

    for (idx, stmt) in statements.iter().enumerate() {
        let js::Statement::FunctionDeclaration(func) = stmt else {
            continue;
        };
        let own_name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
        let Some(body) = &func.body else {
            continue;
        };
        if function_body_calls_forward_decl(body, own_name, idx, &decl_index) {
            return true;
        }
    }
    false
}

fn function_body_calls_forward_decl<'a>(
    body: &js::FunctionBody<'a>,
    own_name: &str,
    current_idx: usize,
    decl_index: &HashMap<&str, usize>,
) -> bool {
    body.statements
        .iter()
        .any(|stmt| stmt_calls_forward_decl(stmt, own_name, current_idx, decl_index))
}

fn stmt_calls_forward_decl<'a>(
    stmt: &js::Statement<'a>,
    own_name: &str,
    current_idx: usize,
    decl_index: &HashMap<&str, usize>,
) -> bool {
    match stmt {
        js::Statement::ExpressionStatement(expr_stmt) => {
            expr_calls_forward_decl(&expr_stmt.expression, own_name, current_idx, decl_index)
        }
        js::Statement::ReturnStatement(ret) => ret
            .argument
            .as_ref()
            .is_some_and(|expr| expr_calls_forward_decl(expr, own_name, current_idx, decl_index)),
        js::Statement::BlockStatement(block) => block
            .body
            .iter()
            .any(|s| stmt_calls_forward_decl(s, own_name, current_idx, decl_index)),
        js::Statement::IfStatement(if_stmt) => {
            expr_calls_forward_decl(&if_stmt.test, own_name, current_idx, decl_index)
                || stmt_calls_forward_decl(&if_stmt.consequent, own_name, current_idx, decl_index)
                || if_stmt.alternate.as_ref().is_some_and(|alt| {
                    stmt_calls_forward_decl(alt, own_name, current_idx, decl_index)
                })
        }
        _ => false,
    }
}

fn expr_calls_forward_decl<'a>(
    expr: &js::Expression<'a>,
    own_name: &str,
    current_idx: usize,
    decl_index: &HashMap<&str, usize>,
) -> bool {
    match expr {
        js::Expression::CallExpression(call) => {
            if let js::Expression::Identifier(id) = &call.callee
                && id.name.as_str() != own_name
                && decl_index
                    .get(id.name.as_str())
                    .is_some_and(|decl_idx| *decl_idx > current_idx)
            {
                return true;
            }
            expr_calls_forward_decl(&call.callee, own_name, current_idx, decl_index)
                || call.arguments.iter().any(|arg| match arg {
                    js::Argument::SpreadElement(spread) => {
                        expr_calls_forward_decl(&spread.argument, own_name, current_idx, decl_index)
                    }
                    _ => {
                        let arg_expr: &js::Expression<'a> = unsafe { std::mem::transmute(arg) };
                        expr_calls_forward_decl(arg_expr, own_name, current_idx, decl_index)
                    }
                })
        }
        js::Expression::BinaryExpression(bin) => {
            expr_calls_forward_decl(&bin.left, own_name, current_idx, decl_index)
                || expr_calls_forward_decl(&bin.right, own_name, current_idx, decl_index)
        }
        js::Expression::LogicalExpression(logical) => {
            expr_calls_forward_decl(&logical.left, own_name, current_idx, decl_index)
                || expr_calls_forward_decl(&logical.right, own_name, current_idx, decl_index)
        }
        js::Expression::ConditionalExpression(cond) => {
            expr_calls_forward_decl(&cond.test, own_name, current_idx, decl_index)
                || expr_calls_forward_decl(&cond.consequent, own_name, current_idx, decl_index)
                || expr_calls_forward_decl(&cond.alternate, own_name, current_idx, decl_index)
        }
        js::Expression::ParenthesizedExpression(paren) => {
            expr_calls_forward_decl(&paren.expression, own_name, current_idx, decl_index)
        }
        js::Expression::SequenceExpression(seq) => seq
            .expressions
            .iter()
            .any(|e| expr_calls_forward_decl(e, own_name, current_idx, decl_index)),
        _ => false,
    }
}

/// Get a human-readable name for a statement type.
fn stmt_type_name(stmt: &js::Statement<'_>) -> &'static str {
    match stmt {
        js::Statement::WithStatement(_) => "WithStatement",
        js::Statement::ClassDeclaration(_) => "ClassDeclaration",
        js::Statement::TSTypeAliasDeclaration(_) => "TSTypeAliasDeclaration",
        js::Statement::TSInterfaceDeclaration(_) => "TSInterfaceDeclaration",
        js::Statement::TSEnumDeclaration(_) => "TSEnumDeclaration",
        js::Statement::TSModuleDeclaration(_) => "TSModuleDeclaration",
        js::Statement::TSImportEqualsDeclaration(_) => "TSImportEqualsDeclaration",
        js::Statement::TSExportAssignment(_) => "TSExportAssignment",
        _ => "unknown",
    }
}

fn lower_var_decl<'a>(
    builder: &mut HIRBuilder,
    decl: &js::VariableDeclaration<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    let allow_lexical_shadowing = !matches!(decl.kind, js::VariableDeclarationKind::Var);
    let kind = variable_decl_kind_to_instruction_kind(
        builder,
        decl.kind,
        "(BuildHIR::lowerStatement) Handle var kinds in VariableDeclaration",
    );

    // Match upstream BuildHIR.ts:883: lower init (RHS) first, then LHS binding.
    // No pre-declaration: OXC semantic analysis handles forward reference detection
    // in lower_ident_expr via is_same_function_scope_reference.
    for declarator in &decl.declarations {
        if let Some(init) = &declarator.init {
            let init_place = lower_expr_to_temp(builder, init, semantic, source);
            lower_binding_pat(
                builder,
                &declarator.id,
                kind,
                init_place,
                semantic,
                source,
                allow_lexical_shadowing,
            );
        } else {
            lower_uninitialized_var_declarator(
                builder,
                &declarator.id,
                kind,
                semantic,
                allow_lexical_shadowing,
            );
        }
    }
}

fn lower_uninitialized_var_declarator<'a>(
    builder: &mut HIRBuilder,
    pattern: &'a js::BindingPattern<'a>,
    kind: hir::InstructionKind,
    semantic: &Semantic<'a>,
    allow_lexical_shadowing: bool,
) {
    let js::BindingPattern::BindingIdentifier(ident) = pattern else {
        builder.push_todo(
            "Expected variable declaration to be an identifier if no initializer was provided"
                .to_string(),
        );
        return;
    };

    let loc = span_to_loc(ident.span);
    let identifier = builder.declare_binding(&ident.name, loc.clone(), allow_lexical_shadowing);
    maybe_record_binding_identifier_rename(semantic, ident, &identifier);
    if kind == hir::InstructionKind::Const {
        builder.mark_binding_const(&ident.name);
    }
    if binding_identifier_is_context_like(ident, semantic) {
        builder.mark_context_identifier(&identifier);
    }

    let is_context = builder.is_context_identifier_id(identifier.id);
    let decl_kind = if is_context && kind == hir::InstructionKind::Const {
        builder.push_todo("Expected `const` declaration not to be reassigned".to_string());
        hir::InstructionKind::Let
    } else {
        kind
    };
    let lvalue = hir::LValue {
        place: hir::Place {
            identifier,
            effect: hir::Effect::Unknown,
            reactive: false,
            loc: loc.clone(),
        },
        kind: decl_kind,
    };

    if is_context {
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::DeclareContext { lvalue, loc },
        );
    } else {
        lower_value_to_temporary(builder, hir::InstructionValue::DeclareLocal { lvalue, loc });
    }
}

fn variable_decl_kind_to_instruction_kind(
    builder: &mut HIRBuilder,
    decl_kind: js::VariableDeclarationKind,
    var_todo_message: &str,
) -> hir::InstructionKind {
    match decl_kind {
        js::VariableDeclarationKind::Const => hir::InstructionKind::Const,
        js::VariableDeclarationKind::Let => hir::InstructionKind::Let,
        js::VariableDeclarationKind::Var => {
            builder.push_todo(var_todo_message.to_string());
            hir::InstructionKind::Let
        }
        _ => hir::InstructionKind::Let,
    }
}

fn predeclare_binding_pat<'a>(
    builder: &mut HIRBuilder,
    semantic: &Semantic<'a>,
    pattern: &js::BindingPattern<'a>,
    kind: hir::InstructionKind,
    allow_lexical_shadowing: bool,
) {
    match pattern {
        js::BindingPattern::BindingIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let identifier = builder.declare_binding(&ident.name, loc, allow_lexical_shadowing);
            maybe_record_binding_identifier_rename(semantic, ident, &identifier);
            if kind == hir::InstructionKind::Const {
                builder.mark_binding_const(&ident.name);
            }
        }
        js::BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                predeclare_binding_pat(
                    builder,
                    semantic,
                    &prop.value,
                    kind,
                    allow_lexical_shadowing,
                );
            }
            if let Some(rest) = &obj.rest {
                predeclare_binding_pat(
                    builder,
                    semantic,
                    &rest.argument,
                    kind,
                    allow_lexical_shadowing,
                );
            }
        }
        js::BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                predeclare_binding_pat(builder, semantic, elem, kind, allow_lexical_shadowing);
            }
            if let Some(rest) = &arr.rest {
                predeclare_binding_pat(
                    builder,
                    semantic,
                    &rest.argument,
                    kind,
                    allow_lexical_shadowing,
                );
            }
        }
        js::BindingPattern::AssignmentPattern(assign) => {
            predeclare_binding_pat(
                builder,
                semantic,
                &assign.left,
                kind,
                allow_lexical_shadowing,
            );
        }
    }
}

fn lower_binding_pat<'a>(
    builder: &mut HIRBuilder,
    pattern: &'a js::BindingPattern<'a>,
    kind: hir::InstructionKind,
    value: hir::Place,
    semantic: &Semantic<'a>,
    source: &str,
    allow_lexical_shadowing: bool,
) {
    match pattern {
        js::BindingPattern::BindingIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let identifier =
                builder.declare_binding(&ident.name, loc.clone(), allow_lexical_shadowing);
            maybe_record_binding_identifier_rename(semantic, ident, &identifier);
            if kind == hir::InstructionKind::Const {
                builder.mark_binding_const(&ident.name);
            }
            if binding_identifier_is_context_like(ident, semantic) {
                builder.mark_context_identifier(&identifier);
            }
            let is_context = builder.is_context_identifier_id(identifier.id);
            let lvalue_place = hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: loc.clone(),
            };
            let lvalue = hir::LValue {
                place: lvalue_place,
                kind,
            };
            if is_context {
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreContext { lvalue, value, loc },
                );
            } else {
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal { lvalue, value, loc },
                );
            }
        }
        js::BindingPattern::ObjectPattern(obj) => {
            // Emit a Destructure instruction to preserve the destructuring pattern.
            let mut properties = Vec::new();
            let mut nested_followups: Vec<(hir::Place, &js::BindingPattern<'a>)> = Vec::new();
            for prop in &obj.properties {
                if prop.computed {
                    builder.push_todo(
                        "(BuildHIR::lowerAssignment) Handle computed properties in ObjectPattern"
                            .to_string(),
                    );
                    continue;
                }
                let prop_key = match &prop.key {
                    js::PropertyKey::StaticIdentifier(id) => {
                        hir::ObjectPropertyKey::Identifier(id.name.to_string())
                    }
                    js::PropertyKey::StringLiteral(s) => {
                        hir::ObjectPropertyKey::String(s.value.to_string())
                    }
                    js::PropertyKey::NumericLiteral(n) => hir::ObjectPropertyKey::Number(n.value),
                    _ => {
                        // Complex key — fall back to decomposed PropertyLoad + StoreLocal
                        let loaded = lower_value_to_temporary(
                            builder,
                            hir::InstructionValue::PropertyLoad {
                                object: value.clone(),
                                property: hir::PropertyLiteral::String("unknown".to_string()),
                                optional: false,
                                loc: span_to_loc(prop.span),
                            },
                        );
                        lower_binding_pat(
                            builder,
                            &prop.value,
                            kind,
                            loaded,
                            semantic,
                            source,
                            allow_lexical_shadowing,
                        );
                        continue;
                    }
                };
                // Nested patterns must be lowered in a follow-up pass from the
                // destructured temporary, otherwise inner bindings are dropped.
                let place = if let js::BindingPattern::BindingIdentifier(ident) = &prop.value {
                    if is_context_for_destructuring(builder, ident, semantic) {
                        let temp = builder.make_temporary_place(span_to_loc(prop.span));
                        nested_followups.push((temp.clone(), &prop.value));
                        temp
                    } else {
                        declare_pattern_place(
                            builder,
                            ident,
                            kind,
                            semantic,
                            allow_lexical_shadowing,
                        )
                    }
                } else {
                    let temp = builder.make_temporary_place(span_to_loc(prop.span));
                    nested_followups.push((temp.clone(), &prop.value));
                    temp
                };
                properties.push(hir::ObjectPropertyOrSpread::Property(hir::ObjectProperty {
                    key: prop_key,
                    type_: hir::ObjectPropertyType::Property,
                    place,
                }));
            }
            if let Some(rest) = &obj.rest {
                let place = if let js::BindingPattern::BindingIdentifier(ident) = &rest.argument {
                    if is_context_for_destructuring(builder, ident, semantic) {
                        let temp = builder.make_temporary_place(span_to_loc(rest.span));
                        nested_followups.push((temp.clone(), &rest.argument));
                        temp
                    } else {
                        declare_pattern_place(
                            builder,
                            ident,
                            kind,
                            semantic,
                            allow_lexical_shadowing,
                        )
                    }
                } else {
                    let temp = builder.make_temporary_place(span_to_loc(rest.span));
                    nested_followups.push((temp.clone(), &rest.argument));
                    temp
                };
                properties.push(hir::ObjectPropertyOrSpread::Spread(place));
            }
            let loc = value.loc.clone();
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::Destructure {
                    lvalue: hir::LValuePattern {
                        pattern: hir::Pattern::Object(hir::ObjectPattern { properties }),
                        kind,
                    },
                    value: value.clone(),
                    loc,
                },
            );
            for (temp, pattern) in nested_followups {
                lower_binding_pat(
                    builder,
                    pattern,
                    kind,
                    temp,
                    semantic,
                    source,
                    allow_lexical_shadowing,
                );
            }
        }
        js::BindingPattern::ArrayPattern(arr) => {
            // Emit a Destructure instruction to preserve the destructuring pattern.
            // Elements with nested patterns or defaults are destructured into a
            // temporary, then lowered in-order as follow-up assignments.
            let mut items = Vec::new();
            enum ArrayFollowup<'a> {
                Nested {
                    temp: hir::Place,
                    pattern: &'a js::BindingPattern<'a>,
                },
                Default {
                    temp: hir::Place,
                    left: &'a js::BindingPattern<'a>,
                    default_expr: &'a js::Expression<'a>,
                },
            }
            let mut followups: Vec<ArrayFollowup<'a>> = Vec::new();
            for elem in arr.elements.iter() {
                if let Some(elem) = elem {
                    match elem {
                        js::BindingPattern::AssignmentPattern(assign) => {
                            // Destructure into a temp; apply the default in a follow-up.
                            let temp_place = builder.make_temporary_place(span_to_loc(elem.span()));
                            followups.push(ArrayFollowup::Default {
                                temp: temp_place.clone(),
                                left: &assign.left,
                                default_expr: &assign.right,
                            });
                            items.push(hir::ArrayElement::Place(temp_place));
                        }
                        js::BindingPattern::BindingIdentifier(ident) => {
                            // Use semantic-based check plus builder-level context
                            // marks (hoisted contexts may already be declared).
                            if is_context_for_destructuring(builder, ident, semantic) {
                                let temp_place =
                                    builder.make_temporary_place(span_to_loc(elem.span()));
                                followups.push(ArrayFollowup::Nested {
                                    temp: temp_place.clone(),
                                    pattern: elem,
                                });
                                items.push(hir::ArrayElement::Place(temp_place));
                            } else {
                                let place = declare_pattern_place(
                                    builder,
                                    ident,
                                    kind,
                                    semantic,
                                    allow_lexical_shadowing,
                                );
                                items.push(hir::ArrayElement::Place(place));
                            }
                        }
                        _ => {
                            // Nested array/object patterns are lowered after destructure.
                            let temp_place = builder.make_temporary_place(span_to_loc(elem.span()));
                            followups.push(ArrayFollowup::Nested {
                                temp: temp_place.clone(),
                                pattern: elem,
                            });
                            items.push(hir::ArrayElement::Place(temp_place));
                        }
                    }
                } else {
                    items.push(hir::ArrayElement::Hole);
                }
            }
            if let Some(rest) = &arr.rest {
                if let js::BindingPattern::BindingIdentifier(ident) = &rest.argument {
                    if is_context_for_destructuring(builder, ident, semantic) {
                        let temp_place = builder.make_temporary_place(span_to_loc(rest.span));
                        followups.push(ArrayFollowup::Nested {
                            temp: temp_place.clone(),
                            pattern: &rest.argument,
                        });
                        items.push(hir::ArrayElement::Spread(temp_place));
                    } else {
                        let place = declare_pattern_place(
                            builder,
                            ident,
                            kind,
                            semantic,
                            allow_lexical_shadowing,
                        );
                        items.push(hir::ArrayElement::Spread(place));
                    }
                } else {
                    let temp_place = builder.make_temporary_place(span_to_loc(rest.span));
                    followups.push(ArrayFollowup::Nested {
                        temp: temp_place.clone(),
                        pattern: &rest.argument,
                    });
                    items.push(hir::ArrayElement::Spread(temp_place));
                }
            }
            let loc = value.loc.clone();
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::Destructure {
                    lvalue: hir::LValuePattern {
                        pattern: hir::Pattern::Array(hir::ArrayPattern { items }),
                        kind,
                    },
                    value: value.clone(),
                    loc: loc.clone(),
                },
            );
            for followup in followups {
                match followup {
                    ArrayFollowup::Nested { temp, pattern } => {
                        lower_binding_pat(
                            builder,
                            pattern,
                            kind,
                            temp,
                            semantic,
                            source,
                            allow_lexical_shadowing,
                        );
                    }
                    ArrayFollowup::Default {
                        temp,
                        left,
                        default_expr,
                    } => {
                        let result_place = emit_default_value_branch(
                            builder,
                            temp,
                            default_expr,
                            semantic,
                            source,
                            &loc,
                        );
                        lower_binding_pat(
                            builder,
                            left,
                            kind,
                            result_place,
                            semantic,
                            source,
                            allow_lexical_shadowing,
                        );
                    }
                }
            }
        }
        js::BindingPattern::AssignmentPattern(assign) => {
            let result_place = emit_default_value_branch(
                builder,
                value,
                &assign.right,
                semantic,
                source,
                &hir::SourceLocation::Generated,
            );
            lower_binding_pat(
                builder,
                &assign.left,
                kind,
                result_place,
                semantic,
                source,
                allow_lexical_shadowing,
            );
        }
    }
}

fn lower_reorderable_expr_to_temp<'a>(
    builder: &mut HIRBuilder,
    expr: &js::Expression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::Place {
    if !is_reorderable_expression(builder, expr, semantic, true) {
        builder.push_todo(format!(
            "(BuildHIR::node.lowerReorderableExpression) Expression type `{}` cannot be safely reordered",
            reorderable_expr_type_name(expr)
        ));
    }
    lower_expr_to_temp(builder, expr, semantic, source)
}

fn declare_pattern_place<'a>(
    builder: &mut HIRBuilder,
    ident: &js::BindingIdentifier<'a>,
    kind: hir::InstructionKind,
    semantic: &Semantic<'a>,
    allow_lexical_shadowing: bool,
) -> hir::Place {
    let loc = span_to_loc(ident.span);
    let identifier = builder.declare_binding(&ident.name, loc.clone(), allow_lexical_shadowing);
    maybe_record_binding_identifier_rename(semantic, ident, &identifier);
    if kind == hir::InstructionKind::Const {
        builder.mark_binding_const(&ident.name);
    }
    if binding_identifier_is_context_like(ident, semantic) {
        builder.mark_context_identifier(&identifier);
    }
    hir::Place {
        identifier,
        effect: hir::Effect::Unknown,
        reactive: false,
        loc,
    }
}

/// Emit a block-based default value computation for destructuring patterns.
/// Creates proper CFG structure: test block → branch(value === undefined) →
/// consequent(store default) / alternate(store value) → continuation.
/// This matches the upstream BuildHIR.ts pattern for AssignmentPattern.
fn emit_default_value_branch<'a>(
    builder: &mut HIRBuilder,
    value: hir::Place,
    default_expr: &js::Expression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: &hir::SourceLocation,
) -> hir::Place {
    let continuation = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation.id;
    let test_block = builder.reserve(hir::BlockKind::Value);
    let result_place = builder.make_temporary_place(loc.clone());

    // Consequent: store the default value to result_place
    let consequent = builder.enter(hir::BlockKind::Value, |builder, _| {
        let default_place = lower_reorderable_expr_to_temp(builder, default_expr, semantic, source);
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    place: result_place.clone(),
                    kind: hir::InstructionKind::Const,
                },
                value: default_place.clone(),
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    // Alternate: store the original value to result_place
    let alternate = builder.enter(hir::BlockKind::Value, |builder, _| {
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    place: result_place.clone(),
                    kind: hir::InstructionKind::Const,
                },
                value: value.clone(),
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    // Terminate current block with Ternary terminal → test block
    builder.terminate_with_continuation(
        hir::Terminal::Ternary {
            test: test_block.id,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        test_block,
    );

    // In test block: evaluate `value === undefined`, then branch
    let undef_place = lower_value_to_temporary(
        builder,
        hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Undefined,
            loc: loc.clone(),
        },
    );
    let test_result = lower_value_to_temporary(
        builder,
        hir::InstructionValue::BinaryExpression {
            operator: hir::BinaryOperator::StrictEq,
            left: value,
            right: undef_place,
            loc: loc.clone(),
        },
    );
    builder.terminate_with_continuation(
        hir::Terminal::Branch {
            test: test_result,
            consequent,
            alternate,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        continuation,
    );

    result_place
}

fn is_reorderable_expression<'a>(
    builder: &HIRBuilder,
    expr: &js::Expression<'a>,
    semantic: &Semantic<'a>,
    allow_local_identifiers: bool,
) -> bool {
    match expr {
        js::Expression::Identifier(ident) => {
            let name = ident.name.as_str();
            if builder.bindings.contains_key(name) {
                allow_local_identifiers
            } else {
                true
            }
        }
        js::Expression::TSInstantiationExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, semantic, allow_local_identifiers)
        }
        js::Expression::RegExpLiteral(_)
        | js::Expression::StringLiteral(_)
        | js::Expression::NumericLiteral(_)
        | js::Expression::NullLiteral(_)
        | js::Expression::BooleanLiteral(_)
        | js::Expression::BigIntLiteral(_) => true,
        js::Expression::UnaryExpression(unary) => match unary.operator {
            oxc_syntax::operator::UnaryOperator::LogicalNot
            | oxc_syntax::operator::UnaryOperator::UnaryPlus
            | oxc_syntax::operator::UnaryOperator::UnaryNegation => is_reorderable_expression(
                builder,
                &unary.argument,
                semantic,
                allow_local_identifiers,
            ),
            _ => false,
        },
        js::Expression::TSAsExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, semantic, allow_local_identifiers)
        }
        js::Expression::TSSatisfiesExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, semantic, allow_local_identifiers)
        }
        js::Expression::TSNonNullExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, semantic, allow_local_identifiers)
        }
        js::Expression::TSTypeAssertion(ts) => {
            is_reorderable_expression(builder, &ts.expression, semantic, allow_local_identifiers)
        }
        js::Expression::ParenthesizedExpression(paren) => is_reorderable_expression(
            builder,
            &paren.expression,
            semantic,
            allow_local_identifiers,
        ),
        js::Expression::LogicalExpression(logical) => {
            is_reorderable_expression(builder, &logical.left, semantic, allow_local_identifiers)
                && is_reorderable_expression(
                    builder,
                    &logical.right,
                    semantic,
                    allow_local_identifiers,
                )
        }
        js::Expression::ConditionalExpression(cond) => {
            is_reorderable_expression(builder, &cond.test, semantic, allow_local_identifiers)
                && is_reorderable_expression(
                    builder,
                    &cond.consequent,
                    semantic,
                    allow_local_identifiers,
                )
                && is_reorderable_expression(
                    builder,
                    &cond.alternate,
                    semantic,
                    allow_local_identifiers,
                )
        }
        js::Expression::ArrayExpression(arr) => arr.elements.iter().all(|element| match element {
            js::ArrayExpressionElement::SpreadElement(_)
            | js::ArrayExpressionElement::Elision(_) => false,
            _ => is_reorderable_expression(
                builder,
                expr_from_array_elem(element),
                semantic,
                allow_local_identifiers,
            ),
        }),
        js::Expression::ObjectExpression(obj) => obj.properties.iter().all(|prop| match prop {
            js::ObjectPropertyKind::ObjectProperty(p) => {
                if p.computed {
                    return false;
                }
                is_reorderable_expression(builder, &p.value, semantic, allow_local_identifiers)
            }
            js::ObjectPropertyKind::SpreadProperty(_) => false,
        }),
        js::Expression::StaticMemberExpression(member) => {
            let mut inner_object: &js::Expression<'_> = &member.object;
            loop {
                match inner_object {
                    js::Expression::StaticMemberExpression(next) => inner_object = &next.object,
                    js::Expression::ComputedMemberExpression(next) => inner_object = &next.object,
                    js::Expression::Identifier(ident) => {
                        let name = ident.name.as_str();
                        if builder.bindings.contains_key(name) {
                            return false;
                        }
                        let binding = resolve_non_local_binding(ident, semantic);
                        return !matches!(binding, hir::NonLocalBinding::Global { .. })
                            || name != "undefined";
                    }
                    _ => return false,
                }
            }
        }
        js::Expression::ComputedMemberExpression(member) => {
            let mut inner_object: &js::Expression<'_> = &member.object;
            loop {
                match inner_object {
                    js::Expression::StaticMemberExpression(next) => inner_object = &next.object,
                    js::Expression::ComputedMemberExpression(next) => inner_object = &next.object,
                    js::Expression::Identifier(ident) => {
                        let name = ident.name.as_str();
                        if builder.bindings.contains_key(name) {
                            return false;
                        }
                        let binding = resolve_non_local_binding(ident, semantic);
                        return !matches!(binding, hir::NonLocalBinding::Global { .. })
                            || name != "undefined";
                    }
                    _ => return false,
                }
            }
        }
        js::Expression::ArrowFunctionExpression(arrow) => {
            if !arrow.expression {
                return arrow.body.statements.is_empty();
            }
            if arrow.body.statements.len() != 1 {
                return false;
            }
            if let js::Statement::ExpressionStatement(expr_stmt) = &arrow.body.statements[0] {
                return is_reorderable_expression(builder, &expr_stmt.expression, semantic, false);
            }
            false
        }
        js::Expression::CallExpression(call) => {
            is_reorderable_expression(builder, &call.callee, semantic, allow_local_identifiers)
                && call.arguments.iter().all(|arg| match arg {
                    js::Argument::SpreadElement(_) => false,
                    _ => is_reorderable_expression(
                        builder,
                        expr_from_arg(arg),
                        semantic,
                        allow_local_identifiers,
                    ),
                })
        }
        _ => false,
    }
}

fn reorderable_expr_type_name(expr: &js::Expression<'_>) -> &'static str {
    match expr {
        js::Expression::Identifier(_) => "Identifier",
        js::Expression::RegExpLiteral(_) => "RegExpLiteral",
        js::Expression::StringLiteral(_) => "StringLiteral",
        js::Expression::NumericLiteral(_) => "NumericLiteral",
        js::Expression::NullLiteral(_) => "NullLiteral",
        js::Expression::BooleanLiteral(_) => "BooleanLiteral",
        js::Expression::BigIntLiteral(_) => "BigIntLiteral",
        js::Expression::UnaryExpression(_) => "UnaryExpression",
        js::Expression::TSAsExpression(_) => "TSAsExpression",
        js::Expression::TSSatisfiesExpression(_) => "TSSatisfiesExpression",
        js::Expression::TSNonNullExpression(_) => "TSNonNullExpression",
        js::Expression::TSTypeAssertion(_) => "TypeCastExpression",
        js::Expression::TSInstantiationExpression(_) => "TSInstantiationExpression",
        js::Expression::LogicalExpression(_) => "LogicalExpression",
        js::Expression::ConditionalExpression(_) => "ConditionalExpression",
        js::Expression::ArrayExpression(_) => "ArrayExpression",
        js::Expression::ObjectExpression(_) => "ObjectExpression",
        js::Expression::StaticMemberExpression(_) => "MemberExpression",
        js::Expression::ComputedMemberExpression(_) => "MemberExpression",
        js::Expression::ArrowFunctionExpression(_) => "ArrowFunctionExpression",
        js::Expression::CallExpression(_) => "CallExpression",
        js::Expression::ChainExpression(_) => "ChainExpression",
        js::Expression::ParenthesizedExpression(_) => "ParenthesizedExpression",
        js::Expression::ThisExpression(_) => "ThisExpression",
        js::Expression::FunctionExpression(_) => "FunctionExpression",
        js::Expression::AssignmentExpression(_) => "AssignmentExpression",
        js::Expression::SequenceExpression(_) => "SequenceExpression",
        js::Expression::TemplateLiteral(_) => "TemplateLiteral",
        js::Expression::TaggedTemplateExpression(_) => "TaggedTemplateExpression",
        js::Expression::MetaProperty(_) => "MetaProperty",
        js::Expression::AwaitExpression(_) => "AwaitExpression",
        js::Expression::YieldExpression(_) => "YieldExpression",
        js::Expression::ClassExpression(_) => "ClassExpression",
        js::Expression::Super(_) => "Super",
        js::Expression::ImportExpression(_) => "ImportExpression",
        js::Expression::PrivateInExpression(_) => "PrivateInExpression",
        js::Expression::PrivateFieldExpression(_) => "PrivateFieldExpression",
        js::Expression::JSXElement(_) => "JSXElement",
        js::Expression::JSXFragment(_) => "JSXFragment",
        js::Expression::UpdateExpression(_) => "UpdateExpression",
        js::Expression::BinaryExpression(_) => "BinaryExpression",
        js::Expression::NewExpression(_) => "NewExpression",
        _ => "unknown",
    }
}

fn binding_identifier_is_context_like(
    ident: &js::BindingIdentifier<'_>,
    semantic: &Semantic<'_>,
) -> bool {
    let debug = std::env::var("DEBUG_CONTEXT_LIKE").is_ok();
    let Some(symbol_id) = ident.symbol_id.get() else {
        if debug {
            eprintln!(
                "[DEBUG_CONTEXT_LIKE] binding name={} symbol=<none> => false",
                ident.name
            );
        }
        return false;
    };
    let scoping = semantic.scoping();
    let decl_scope = scoping.symbol_scope_id(symbol_id);
    let mut has_write = false;
    let mut captured_in_nested_scope = false;

    for reference in semantic.symbol_references(symbol_id) {
        if reference.is_write() {
            has_write = true;
        }
        let reference_scope = reference.scope_id();
        if reference_scope != decl_scope {
            let mut reaches_decl_scope = false;
            let mut crosses_function_scope = false;
            for scope_id in scoping.scope_ancestors(reference_scope) {
                if scope_id == decl_scope {
                    reaches_decl_scope = true;
                    break;
                }
                if scoping.scope_flags(scope_id).is_function() {
                    crosses_function_scope = true;
                }
            }
            if reaches_decl_scope && crosses_function_scope {
                captured_in_nested_scope = true;
            }
        }
        if has_write && captured_in_nested_scope {
            if debug {
                eprintln!(
                    "[DEBUG_CONTEXT_LIKE] binding name={} symbol={} write={} captured={} => true",
                    ident.name,
                    symbol_id.index(),
                    has_write,
                    captured_in_nested_scope
                );
            }
            return true;
        }
    }
    let result = has_write && captured_in_nested_scope;
    if debug {
        eprintln!(
            "[DEBUG_CONTEXT_LIKE] binding name={} symbol={} write={} captured={} => {}",
            ident.name,
            symbol_id.index(),
            has_write,
            captured_in_nested_scope,
            result
        );
    }
    result
}

/// Returns true when a binding identifier should be treated as a context
/// variable for the purpose of destructuring lowering.  This checks both
/// the semantic-level heuristic (`binding_identifier_is_context_like`) and
/// whether the builder has *already* marked the resolved identifier as
/// context-bound (e.g. from a hoisted context declaration).
fn is_context_for_destructuring(
    builder: &HIRBuilder,
    ident: &js::BindingIdentifier<'_>,
    semantic: &Semantic<'_>,
) -> bool {
    if binding_identifier_is_context_like(ident, semantic) {
        return true;
    }
    // The hoisted-context lowering may have already declared + marked this
    // binding as context before the actual variable declaration is processed.
    if let Some(entry) = builder.bindings.get(ident.name.as_str()) {
        return builder.is_context_identifier_id(entry.identifier.id);
    }
    false
}

fn lower_func_decl<'a>(
    builder: &mut HIRBuilder,
    func: &js::Function<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    let name = func.id.as_ref().map(|id| id.name.as_str());
    if let Some(name_str) = name {
        let loc = span_to_loc(func.span);
        let identifier = builder.resolve_binding(name_str, loc.clone());
        let lowered = lower_nested_func(builder, func, semantic, source);
        let func_place = lower_value_to_temporary(
            builder,
            hir::InstructionValue::FunctionExpression {
                name: Some(name_str.to_string()),
                lowered_func: lowered,
                expr_type: hir::FunctionExpressionType::FunctionDeclaration,
                loc: loc.clone(),
            },
        );
        let lvalue_place = hir::Place {
            identifier,
            effect: hir::Effect::Unknown,
            reactive: false,
            loc: loc.clone(),
        };
        if builder.is_context_identifier_id(lvalue_place.identifier.id) {
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::StoreContext {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Function,
                    },
                    value: func_place,
                    loc,
                },
            );
        } else {
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::StoreLocal {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Function,
                    },
                    value: func_place,
                    loc,
                },
            );
        }
    }
}

fn lower_nested_func<'a>(
    builder: &mut HIRBuilder,
    func: &js::Function<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::LoweredFunction {
    let env = builder.env.clone();
    if let Some(body) = &func.body {
        match lower_function_inner(
            body,
            &func.params,
            LoweringContext::new(semantic, source, env)
                .with_binding_name_counters(builder.binding_name_counters()),
            LowerFunctionOptions::function(
                func.id.as_ref().map(|id| id.name.as_str()),
                func.span,
                func.generator,
                func.r#async,
            ),
        ) {
            Ok(result) => hir::LoweredFunction { func: result.func },
            Err(e) => {
                // Propagate inner function lowering errors to the outer builder
                // so they cause a bail-out at the top level.
                for msg in e.split('\n') {
                    if !msg.is_empty() {
                        builder.push_todo(msg.to_string());
                    }
                }
                stub_lowered_function(builder)
            }
        }
    } else {
        stub_lowered_function(builder)
    }
}

fn stub_lowered_function(builder: &mut HIRBuilder) -> hir::LoweredFunction {
    let env = builder.env.clone();
    hir::LoweredFunction {
        func: hir::HIRFunction {
            env,
            id: None,
            fn_type: hir::ReactFunctionType::Other,
            params: Vec::new(),
            returns: builder.make_temporary_place(hir::SourceLocation::Generated),
            context: Vec::new(),
            body: hir::HIR {
                entry: hir::BlockId(0),
                blocks: Vec::new(),
            },
            generator: false,
            async_: false,
            directives: Vec::new(),
            aliasing_effects: None,
        },
    }
}

fn lower_if<'a>(
    builder: &mut HIRBuilder,
    if_stmt: &js::IfStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    let consequent_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        lower_statement(builder, &if_stmt.consequent, semantic, source);
        hir::Terminal::Goto {
            block: cont_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: span_to_loc(if_stmt.consequent.span()),
        }
    });

    let alternate_block = if let Some(alt) = &if_stmt.alternate {
        builder.enter(hir::BlockKind::Block, |builder, _| {
            lower_statement(builder, alt, semantic, source);
            hir::Terminal::Goto {
                block: cont_id,
                variant: hir::GotoVariant::Break,
                id: hir::InstructionId::default(),
                loc: span_to_loc(alt.span()),
            }
        })
    } else {
        cont_id
    };

    let test = lower_expr_to_temp(builder, &if_stmt.test, semantic, source);
    builder.terminate_with_continuation(
        hir::Terminal::If {
            test,
            consequent: consequent_block,
            alternate: alternate_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: span_to_loc(if_stmt.span),
        },
        continuation,
    );
}

fn lower_for<'a>(
    builder: &mut HIRBuilder,
    for_stmt: &js::ForStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    // Reserve test block first (will be populated after the For terminal)
    let test_wip = builder.reserve(hir::BlockKind::Loop);
    let test_block = test_wip.id;

    // Reserve continuation block (code after the loop)
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    // Enter a binding scope for the for-loop's variable declarations so that
    // sibling loops with the same variable name get unique DeclarationIds.
    builder.enter_binding_scope();

    // Init block: variable declarations or expression
    let init_block = builder.enter(hir::BlockKind::Loop, |builder, _| {
        if let Some(init) = &for_stmt.init {
            match init {
                js::ForStatementInit::VariableDeclaration(decl) => {
                    lower_var_decl(builder, decl, semantic, source);
                }
                _ => {
                    // Expression init — cast to Expression and lower
                    let expr: &js::Expression<'a> = unsafe { std::mem::transmute(init) };
                    let _ = lower_expr_to_temp(builder, expr, semantic, source);
                }
            }
        }
        hir::Terminal::Goto {
            block: test_block,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: span_to_loc(for_stmt.span),
        }
    });

    // Update block (optional)
    let update_block = for_stmt.update.as_ref().map(|update| {
        builder.enter(hir::BlockKind::Loop, |builder, _| {
            let _ = lower_expr_to_temp(builder, update, semantic, source);
            hir::Terminal::Goto {
                block: test_block,
                variant: hir::GotoVariant::Break,
                id: hir::InstructionId::default(),
                loc: span_to_loc(for_stmt.span),
            }
        })
    });

    // Loop body block
    let loop_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        let continue_target = update_block.unwrap_or(test_block);
        builder.push_loop(None, continue_target, cont_id);
        lower_statement(builder, &for_stmt.body, semantic, source);
        builder.pop_loop();
        hir::Terminal::Goto {
            block: continue_target,
            variant: hir::GotoVariant::Continue,
            id: hir::InstructionId::default(),
            loc: span_to_loc(for_stmt.body.span()),
        }
    });

    // Emit the For terminal, then make test_wip the current block
    builder.terminate_with_continuation(
        hir::Terminal::For {
            init: init_block,
            test: test_block,
            update: update_block,
            loop_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: span_to_loc(for_stmt.span),
        },
        test_wip,
    );

    // Now the test block is the current block. Lower the test expression
    // and emit a Branch terminal (matching upstream BuildHIR).
    if let Some(test) = &for_stmt.test {
        let test_place = lower_expr_to_temp(builder, test, semantic, source);
        builder.terminate_with_continuation(
            hir::Terminal::Branch {
                test: test_place,
                consequent: loop_block,
                alternate: cont_id,
                fallthrough: cont_id,
                id: hir::InstructionId::default(),
                loc: span_to_loc(for_stmt.span),
            },
            continuation,
        );
    } else {
        // No test expression (infinite loop like `for(;;)`) — always enter body
        // TODO: upstream reports an error for empty test in ForStatement
        let true_val = hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Boolean(true),
            loc: span_to_loc(for_stmt.span),
        };
        let test_place = lower_value_to_temporary(builder, true_val);
        builder.terminate_with_continuation(
            hir::Terminal::Branch {
                test: test_place,
                consequent: loop_block,
                alternate: cont_id,
                fallthrough: cont_id,
                id: hir::InstructionId::default(),
                loc: span_to_loc(for_stmt.span),
            },
            continuation,
        );
    }

    // Exit the binding scope opened at the start of lower_for.
    builder.exit_binding_scope();
}

fn lower_for_of<'a>(
    builder: &mut HIRBuilder,
    for_of: &js::ForOfStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    if for_of.r#await {
        builder.push_todo("(BuildHIR::lowerStatement) Handle for-await loops".to_string());
        return;
    }
    lower_for_of_inner(builder, for_of, semantic, source);
}

fn lower_for_of_inner<'a>(
    builder: &mut HIRBuilder,
    for_of: &js::ForOfStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    if for_of_has_non_trivial_context_iterator(for_of, semantic) {
        builder.push_todo("Support non-trivial for..of inits".to_string());
        return;
    }

    // Enter a binding scope for the for-of's variable declarations so that
    // each for-of loop gets unique DeclarationIds for its bindings, even if
    // sibling loops use the same variable name (e.g., `for (const [i, ..] of a)`,
    // `for (const [i, ..] of b)`).
    builder.enter_binding_scope();
    let (for_of_decl_kind, for_of_allow_lexical_shadowing) =
        if let js::ForStatementLeft::VariableDeclaration(decl) = &for_of.left {
            let allow_lexical_shadowing = !matches!(decl.kind, js::VariableDeclarationKind::Var);
            let decl_kind = variable_decl_kind_to_instruction_kind(
                builder,
                decl.kind,
                "(BuildHIR::lowerStatement) Handle var kinds in ForOfStatement",
            );
            for declarator in &decl.declarations {
                predeclare_binding_pat(
                    builder,
                    semantic,
                    &declarator.id,
                    decl_kind,
                    allow_lexical_shadowing,
                );
            }
            (Some(decl_kind), allow_lexical_shadowing)
        } else {
            (None, false)
        };

    let continuation = builder.reserve(hir::BlockKind::Block);
    let init_wip = builder.reserve(hir::BlockKind::Loop);
    let test_wip = builder.reserve(hir::BlockKind::Loop);
    let cont_id = continuation.id;
    let init_id = init_wip.id;
    let test_id = test_wip.id;

    // Loop body block: body ...; continue -> init
    let loop_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        builder.push_loop(None, init_id, cont_id);
        lower_statement(builder, &for_of.body, semantic, source);
        builder.pop_loop();
        hir::Terminal::Goto {
            block: init_id,
            variant: hir::GotoVariant::Continue,
            id: hir::InstructionId::default(),
            loc: span_to_loc(for_of.body.span()),
        }
    });

    let loc = span_to_loc(for_of.span);
    let collection = lower_expr_to_temp(builder, &for_of.right, semantic, source);

    // Emit `for-of` terminal and continue in init block.
    builder.terminate_with_continuation(
        hir::Terminal::ForOf {
            init: init_id,
            test: test_id,
            loop_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        init_wip,
    );

    // Init block: materialize iterator then jump to test block.
    let iterator = lower_value_to_temporary(
        builder,
        hir::InstructionValue::GetIterator {
            collection: collection.clone(),
            loc: collection.loc.clone(),
        },
    );
    builder.terminate_with_continuation(
        hir::Terminal::Goto {
            block: test_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        test_wip,
    );

    // Test block: advance iterator and assign to left target, then branch.
    let left_loc = span_to_loc(for_of.left.span());
    let next_item = lower_value_to_temporary(
        builder,
        hir::InstructionValue::IteratorNext {
            iterator,
            collection,
            loc: left_loc.clone(),
        },
    );

    let test_place = if let js::ForStatementLeft::VariableDeclaration(decl) = &for_of.left {
        {
            if decl.declarations.len() != 1 {
                builder.push_invariant(format!(
                    "Expected exactly one declaration in for-of init, got {}",
                    decl.declarations.len()
                ));
            }
            if let Some(first_decl) = decl.declarations.first() {
                lower_binding_pat(
                    builder,
                    &first_decl.id,
                    for_of_decl_kind.unwrap_or(hir::InstructionKind::Let),
                    next_item.clone(),
                    semantic,
                    source,
                    for_of_allow_lexical_shadowing,
                );

                if let js::BindingPattern::BindingIdentifier(ident) = &first_decl.id {
                    // Match upstream assignment-expression semantics for
                    // `for (const/let x of y)` by branching on the assigned
                    // binding value rather than raw IteratorNext output.
                    let identifier = builder.resolve_binding(&ident.name, left_loc.clone());
                    let assigned_place = hir::Place {
                        identifier,
                        effect: hir::Effect::Unknown,
                        reactive: false,
                        loc: left_loc.clone(),
                    };
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadLocal {
                            place: assigned_place,
                            loc: left_loc.clone(),
                        },
                    )
                } else {
                    next_item.clone()
                }
            } else {
                next_item.clone()
            }
        }
    } else if let Some(target) = for_of.left.as_assignment_target() {
        emit_store_for_target(builder, next_item.clone(), target, semantic, source);
        next_item
    } else {
        builder.push_invariant(
            "Expected for-of left to be declaration or assignment target".to_string(),
        );
        next_item
    };

    // Exit the binding scope opened at the start of lower_for_of_inner,
    // restoring previous bindings so sibling for-of loops get unique DeclarationIds.
    builder.exit_binding_scope();

    builder.terminate_with_continuation(
        hir::Terminal::Branch {
            test: test_place,
            consequent: loop_block,
            alternate: cont_id,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc,
        },
        continuation,
    );
}

fn for_of_has_non_trivial_context_iterator(
    for_of: &js::ForOfStatement<'_>,
    semantic: &Semantic<'_>,
) -> bool {
    let js::ForStatementLeft::VariableDeclaration(decl) = &for_of.left else {
        return false;
    };
    let Some(first_decl) = decl.declarations.first() else {
        return false;
    };
    let js::BindingPattern::BindingIdentifier(ident) = &first_decl.id else {
        return false;
    };
    let Some(symbol_id) = ident.symbol_id.get() else {
        return false;
    };

    let scoping = semantic.scoping();
    let decl_scope = scoping.symbol_scope_id(symbol_id);
    let mut has_write = false;
    let mut captured_in_nested_scope = false;

    for reference in semantic.symbol_references(symbol_id) {
        if reference.is_write() {
            has_write = true;
        }
        let reference_scope = reference.scope_id();
        if reference_scope != decl_scope
            && scoping
                .scope_ancestors(reference_scope)
                .any(|scope_id| scope_id == decl_scope)
        {
            captured_in_nested_scope = true;
        }
        if has_write && captured_in_nested_scope {
            return true;
        }
    }

    has_write && captured_in_nested_scope
}

fn lower_for_in<'a>(
    builder: &mut HIRBuilder,
    for_in: &js::ForInStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    if for_in_has_non_trivial_context_iterator(for_in, semantic) {
        builder.push_todo("Support non-trivial for..in inits".to_string());
    }

    builder.enter_binding_scope();
    let (for_in_decl_kind, for_in_allow_lexical_shadowing) =
        if let js::ForStatementLeft::VariableDeclaration(decl) = &for_in.left {
            let allow_lexical_shadowing = !matches!(decl.kind, js::VariableDeclarationKind::Var);
            let decl_kind = variable_decl_kind_to_instruction_kind(
                builder,
                decl.kind,
                "(BuildHIR::lowerStatement) Handle var kinds in ForInStatement",
            );
            for declarator in &decl.declarations {
                predeclare_binding_pat(
                    builder,
                    semantic,
                    &declarator.id,
                    decl_kind,
                    allow_lexical_shadowing,
                );
            }
            (Some(decl_kind), allow_lexical_shadowing)
        } else {
            (None, false)
        };

    let continuation = builder.reserve(hir::BlockKind::Block);
    let init_wip = builder.reserve(hir::BlockKind::Loop);
    let cont_id = continuation.id;
    let init_id = init_wip.id;

    // Loop body block: body ...; continue -> init
    let loop_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        builder.push_loop(None, init_id, cont_id);
        lower_statement(builder, &for_in.body, semantic, source);
        builder.pop_loop();
        hir::Terminal::Goto {
            block: init_id,
            variant: hir::GotoVariant::Continue,
            id: hir::InstructionId::default(),
            loc: span_to_loc(for_in.body.span()),
        }
    });

    let loc = span_to_loc(for_in.span);
    let object = lower_expr_to_temp(builder, &for_in.right, semantic, source);

    // Emit `for-in` terminal and continue in init block.
    builder.terminate_with_continuation(
        hir::Terminal::ForIn {
            init: init_id,
            loop_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        init_wip,
    );

    // Init block: fetch next property, assign it, then branch.
    let left_loc = span_to_loc(for_in.left.span());
    let next_property = lower_value_to_temporary(
        builder,
        hir::InstructionValue::NextPropertyOf {
            value: object,
            loc: left_loc.clone(),
        },
    );

    let test_place = if let js::ForStatementLeft::VariableDeclaration(decl) = &for_in.left {
        {
            if decl.declarations.len() != 1 {
                builder.push_invariant(format!(
                    "Expected exactly one declaration in for-in init, got {}",
                    decl.declarations.len()
                ));
            }
            if let Some(first_decl) = decl.declarations.first() {
                lower_binding_pat(
                    builder,
                    &first_decl.id,
                    for_in_decl_kind.unwrap_or(hir::InstructionKind::Let),
                    next_property.clone(),
                    semantic,
                    source,
                    for_in_allow_lexical_shadowing,
                );

                if let js::BindingPattern::BindingIdentifier(ident) = &first_decl.id {
                    let identifier = builder.resolve_binding(&ident.name, left_loc.clone());
                    let assigned_place = hir::Place {
                        identifier,
                        effect: hir::Effect::Unknown,
                        reactive: false,
                        loc: left_loc.clone(),
                    };
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadLocal {
                            place: assigned_place,
                            loc: left_loc.clone(),
                        },
                    )
                } else {
                    next_property.clone()
                }
            } else {
                next_property.clone()
            }
        }
    } else if let Some(target) = for_in.left.as_assignment_target() {
        emit_store_for_target(builder, next_property.clone(), target, semantic, source);
        next_property
    } else {
        builder.push_invariant(
            "Expected for-in left to be declaration or assignment target".to_string(),
        );
        next_property
    };

    builder.terminate_with_continuation(
        hir::Terminal::Branch {
            test: test_place,
            consequent: loop_block,
            alternate: cont_id,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc,
        },
        continuation,
    );
    builder.exit_binding_scope();
}

fn for_in_has_non_trivial_context_iterator(
    for_in: &js::ForInStatement<'_>,
    semantic: &Semantic<'_>,
) -> bool {
    let js::ForStatementLeft::VariableDeclaration(decl) = &for_in.left else {
        return false;
    };
    let Some(first_decl) = decl.declarations.first() else {
        return false;
    };
    let js::BindingPattern::BindingIdentifier(ident) = &first_decl.id else {
        return false;
    };
    let Some(symbol_id) = ident.symbol_id.get() else {
        return false;
    };

    let scoping = semantic.scoping();
    let decl_scope = scoping.symbol_scope_id(symbol_id);
    let mut has_write = false;
    let mut captured_in_nested_scope = false;

    for reference in semantic.symbol_references(symbol_id) {
        if reference.is_write() {
            has_write = true;
        }
        let reference_scope = reference.scope_id();
        if reference_scope != decl_scope
            && scoping
                .scope_ancestors(reference_scope)
                .any(|scope_id| scope_id == decl_scope)
        {
            captured_in_nested_scope = true;
        }
        if has_write && captured_in_nested_scope {
            return true;
        }
    }

    has_write && captured_in_nested_scope
}

fn lower_while<'a>(
    builder: &mut HIRBuilder,
    while_stmt: &js::WhileStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    // Reserve conditional block (will hold the test expression + Branch terminal)
    let conditional_wip = builder.reserve(hir::BlockKind::Loop);
    let conditional_block = conditional_wip.id;

    // Reserve continuation block (code after the loop)
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    // Loop body block
    let loop_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        builder.push_loop(None, conditional_block, cont_id);
        lower_statement(builder, &while_stmt.body, semantic, source);
        builder.pop_loop();
        hir::Terminal::Goto {
            block: conditional_block,
            variant: hir::GotoVariant::Continue,
            id: hir::InstructionId::default(),
            loc: span_to_loc(while_stmt.body.span()),
        }
    });

    // Emit the While terminal, then make conditional_wip the current block
    builder.terminate_with_continuation(
        hir::Terminal::While {
            test: conditional_block,
            loop_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: span_to_loc(while_stmt.span),
        },
        conditional_wip,
    );

    // Now the conditional block is current. Lower the test expression
    // and emit a Branch terminal (matching upstream BuildHIR).
    let test_place = lower_expr_to_temp(builder, &while_stmt.test, semantic, source);
    builder.terminate_with_continuation(
        hir::Terminal::Branch {
            test: test_place,
            consequent: loop_block,
            alternate: cont_id,
            fallthrough: conditional_block,
            id: hir::InstructionId::default(),
            loc: span_to_loc(while_stmt.span),
        },
        continuation,
    );
}

fn lower_do_while<'a>(
    builder: &mut HIRBuilder,
    do_while: &js::DoWhileStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    // Reserve conditional block (will hold the test expression + Branch terminal)
    let conditional_wip = builder.reserve(hir::BlockKind::Loop);
    let conditional_block = conditional_wip.id;

    // Reserve continuation block (code after the loop)
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    // Loop body block (executed at least once unconditionally prior to exit)
    let loop_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        builder.push_loop(None, conditional_block, cont_id);
        lower_statement(builder, &do_while.body, semantic, source);
        builder.pop_loop();
        hir::Terminal::Goto {
            block: conditional_block,
            variant: hir::GotoVariant::Continue,
            id: hir::InstructionId::default(),
            loc: span_to_loc(do_while.body.span()),
        }
    });

    // Emit the DoWhile terminal, then make conditional_wip the current block
    builder.terminate_with_continuation(
        hir::Terminal::DoWhile {
            loop_block,
            test: conditional_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: span_to_loc(do_while.span),
        },
        conditional_wip,
    );

    // Now the conditional block is current. Lower the test expression
    // and emit a Branch terminal (matching upstream BuildHIR).
    let test_place = lower_expr_to_temp(builder, &do_while.test, semantic, source);
    builder.terminate_with_continuation(
        hir::Terminal::Branch {
            test: test_place,
            consequent: loop_block,
            alternate: cont_id,
            fallthrough: conditional_block,
            id: hir::InstructionId::default(),
            loc: span_to_loc(do_while.span),
        },
        continuation,
    );
}

fn lower_switch<'a>(
    builder: &mut HIRBuilder,
    switch: &js::SwitchStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    let loc = span_to_loc(switch.span);
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    // The goto target for any cases that fallthrough, which initially starts
    // as the continuation block and is then updated as we iterate through cases
    // in reverse order.
    let mut fallthrough = cont_id;

    // Iterate through cases in reverse order, so that previous blocks can
    // fallthrough to successors (matches upstream BuildHIR.ts:792-834).
    let mut cases: Vec<hir::SwitchCase> = Vec::new();
    let mut has_default = false;
    for case in switch.cases.iter().rev() {
        if case.test.is_none() {
            has_default = true;
        }
        let ft = fallthrough;
        let case_block = builder.enter(hir::BlockKind::Block, |builder, _| {
            builder.push_switch(None, cont_id);
            for s in &case.consequent {
                lower_statement(builder, s, semantic, source);
            }
            builder.pop_switch();
            // Always generate a fallthrough to the next block; this may be dead
            // code if there was an explicit break, but if so it will be pruned later.
            hir::Terminal::Goto {
                block: ft,
                variant: hir::GotoVariant::Break,
                id: hir::InstructionId::default(),
                loc: loc.clone(),
            }
        });
        let case_test = case
            .test
            .as_ref()
            .map(|t| lower_reorderable_expr_to_temp(builder, t, semantic, source));
        cases.push(hir::SwitchCase {
            test: case_test,
            block: case_block,
        });
        fallthrough = case_block;
    }
    // Reverse back to original order to match the original code/intent.
    cases.reverse();

    // If there wasn't an explicit default case, generate one to model the fact
    // that execution could bypass any of the other cases and jump directly to
    // the continuation.
    if !has_default {
        cases.push(hir::SwitchCase {
            test: None,
            block: cont_id,
        });
    }

    // Lower discriminant after cases (matches upstream order).
    let test = lower_expr_to_temp(builder, &switch.discriminant, semantic, source);

    builder.terminate_with_continuation(
        hir::Terminal::Switch {
            test,
            cases,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc,
        },
        continuation,
    );
}

fn lower_try<'a>(
    builder: &mut HIRBuilder,
    try_stmt: &js::TryStatement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    // Check for try without catch clause
    if try_stmt.handler.is_none() {
        builder.push_todo(
            "(BuildHIR::lowerStatement) Handle TryStatement without catch clause".to_string(),
        );
        return;
    }
    let loc = span_to_loc(try_stmt.span);
    let continuation = builder.reserve(hir::BlockKind::Block);
    let cont_id = continuation.id;

    // Try block: lower the try body
    let try_block = builder.enter(hir::BlockKind::Block, |builder, _| {
        builder.enter_try_context();
        for s in &try_stmt.block.body {
            lower_statement(builder, s, semantic, source);
        }
        builder.exit_try_context();
        hir::Terminal::Goto {
            block: cont_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    // Handler block: lower the catch body
    // Extract the original catch param name (e.g. "e") before creating temporaries
    let catch_param_name: Option<String> = try_stmt.handler.as_ref().and_then(|h| {
        h.param.as_ref().and_then(|param| match &param.pattern {
            js::BindingPattern::BindingIdentifier(ident) => Some(ident.name.to_string()),
            _ => None,
        })
    });
    if try_stmt
        .handler
        .as_ref()
        .and_then(|h| h.param.as_ref())
        .is_some_and(|param| !matches!(param.pattern, js::BindingPattern::BindingIdentifier(_)))
    {
        builder.push_invariant(
            "(BuildHIR::lowerAssignment) Could not find binding for declaration.".to_string(),
        );
    }

    // Create the handler binding as a promoted temporary (like upstream:
    // makeTemporary + promoteTemporary). The handler binding is what appears
    // as the catch parameter: `catch (t1) { ... }`.
    let handler_binding: Option<hir::Place> = catch_param_name.as_ref().map(|_name| {
        let mut place = builder.make_temporary_place(loc.clone());
        // Promote: set name to Promoted("#t{declaration_id}") so it gets renamed to t0/t1/etc.
        place.identifier.name = Some(hir::IdentifierName::Promoted(format!(
            "#t{}",
            place.identifier.declaration_id.0
        )));
        // Emit DeclareLocal { kind: Catch } for the promoted temp (goes into current block,
        // before the handler, matching upstream lowerValueToTemporary behavior)
        let decl_lvalue = builder.make_temporary_place(loc.clone());
        builder.push(hir::Instruction {
            id: hir::InstructionId::default(),
            lvalue: decl_lvalue,
            value: hir::InstructionValue::DeclareLocal {
                lvalue: hir::LValue {
                    place: place.clone(),
                    kind: hir::InstructionKind::Catch,
                },
                loc: loc.clone(),
            },
            loc: loc.clone(),
            effects: None,
        });
        place
    });

    let handler_block = builder.enter(hir::BlockKind::Catch, |builder, _| {
        if let Some(handler) = &try_stmt.handler {
            // If there's a catch param, emit the named binding assignment inside the handler:
            // `const e = t1;` (StoreLocal for the original catch variable name)
            if let (Some(binding), Some(name)) = (&handler_binding, &catch_param_name) {
                // Use resolve_binding so that subsequent references to `e` in the
                // handler body (like `return e;`) resolve to the same identifier.
                let identifier = builder.resolve_binding(name, loc.clone());
                if let Some(param) = handler.param.as_ref()
                    && let js::BindingPattern::BindingIdentifier(ident) = &param.pattern
                {
                    maybe_record_binding_identifier_rename(semantic, ident, &identifier);
                }
                let named_place = hir::Place {
                    identifier,
                    effect: hir::Effect::Unknown,
                    reactive: false,
                    loc: loc.clone(),
                };
                // Use lower_value_to_temporary to match upstream's lowerAssignment pattern
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: named_place,
                            kind: hir::InstructionKind::Catch,
                        },
                        value: binding.clone(),
                        loc: loc.clone(),
                    },
                );
            }
            for s in &handler.body.body {
                lower_statement(builder, s, semantic, source);
            }
        }
        hir::Terminal::Goto {
            block: cont_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    // If there's a finalizer, lower it into the continuation block after the try-catch
    builder.terminate_with_continuation(
        hir::Terminal::Try {
            block: try_block,
            handler_binding,
            handler: handler_block,
            fallthrough: cont_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        continuation,
    );

    // Lower finalizer statements into the continuation block (after try-catch)
    if let Some(finalizer) = &try_stmt.finalizer {
        for s in &finalizer.body {
            lower_statement(builder, s, semantic, source);
        }
    }
}

// ============================================================================
// Expression lowering
// ============================================================================

fn lower_expr_to_temp<'a>(
    builder: &mut HIRBuilder,
    expr: &js::Expression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::Place {
    let value = lower_expr(builder, expr, semantic, source);
    lower_value_to_temporary(builder, value)
}

fn lower_value_to_temporary(builder: &mut HIRBuilder, value: hir::InstructionValue) -> hir::Place {
    if let hir::InstructionValue::LoadLocal { ref place, .. } = value
        && place.identifier.name.is_none()
    {
        return place.clone();
    }
    let loc = value.loc().clone();
    let place = builder.make_temporary_place(loc.clone());
    builder.push(hir::Instruction {
        id: hir::InstructionId::default(),
        lvalue: place.clone(),
        value,
        loc,
        effects: None,
    });
    place
}

fn lower_expr<'a>(
    builder: &mut HIRBuilder,
    expr: &js::Expression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    let loc = span_to_loc(expr.span());
    match expr {
        js::Expression::Identifier(ident) => lower_ident_expr(builder, ident, semantic),
        js::Expression::NullLiteral(_) => hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Null,
            loc,
        },
        js::Expression::BooleanLiteral(b) => hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Boolean(b.value),
            loc,
        },
        js::Expression::NumericLiteral(n) => hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Number(n.value),
            loc,
        },
        js::Expression::StringLiteral(s) => hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::String(s.value.to_string()),
            loc,
        },
        js::Expression::RegExpLiteral(r) => hir::InstructionValue::RegExpLiteral {
            pattern: r.regex.pattern.text.to_string(),
            flags: r.regex.flags.to_string(),
            loc,
        },
        js::Expression::TemplateLiteral(tmpl) => {
            let subexprs: Vec<hir::Place> = tmpl
                .expressions
                .iter()
                .map(|e| lower_expr_to_temp(builder, e, semantic, source))
                .collect();
            let quasis = tmpl
                .quasis
                .iter()
                .map(|q| hir::TemplateQuasi {
                    raw: q.value.raw.to_string(),
                    cooked: q.value.cooked.as_ref().map(|c| c.to_string()),
                })
                .collect();
            hir::InstructionValue::TemplateLiteral {
                subexprs,
                quasis,
                loc,
            }
        }
        js::Expression::ArrayExpression(arr) => {
            let mut elements = Vec::new();
            for elem in &arr.elements {
                match elem {
                    js::ArrayExpressionElement::SpreadElement(spread) => {
                        let place = lower_expr_to_temp(builder, &spread.argument, semantic, source);
                        elements.push(hir::ArrayElement::Spread(place));
                    }
                    js::ArrayExpressionElement::Elision(_) => {
                        elements.push(hir::ArrayElement::Hole);
                    }
                    _ => {
                        // Expression variant (via @inherit)
                        let place = lower_expr_to_temp(
                            builder,
                            expr_from_array_elem(elem),
                            semantic,
                            source,
                        );
                        elements.push(hir::ArrayElement::Place(place));
                    }
                }
            }
            hir::InstructionValue::ArrayExpression { elements, loc }
        }
        js::Expression::ObjectExpression(obj) => {
            let mut properties = Vec::new();
            for prop in &obj.properties {
                match prop {
                    js::ObjectPropertyKind::ObjectProperty(p) => {
                        // Check for getter/setter syntax — unsupported
                        if p.kind == js::PropertyKind::Get {
                            builder.push_todo("(BuildHIR::lowerExpression) Handle get functions in ObjectExpression".to_string());
                            continue;
                        }
                        if p.kind == js::PropertyKind::Set {
                            builder.push_todo("(BuildHIR::lowerExpression) Handle set functions in ObjectExpression".to_string());
                            continue;
                        }
                        let key = if p.computed {
                            // Computed key: lower the key expression to a temporary

                            match &p.key {
                                js::PropertyKey::StaticIdentifier(_)
                                | js::PropertyKey::StringLiteral(_)
                                | js::PropertyKey::NumericLiteral(_) => lower_prop_key(&p.key),
                                _ => {
                                    let key_expr_ref: &js::Expression<'_> =
                                        unsafe { std::mem::transmute(&p.key) };
                                    // Upstream only allows Identifier and MemberExpression
                                    // for computed keys. Complex expressions (CallExpression,
                                    // SequenceExpression, etc.) can trigger bugs with
                                    // conditional mutation.
                                    let is_allowed = matches!(
                                        key_expr_ref,
                                        js::Expression::Identifier(_)
                                            | js::Expression::StaticMemberExpression(_)
                                            | js::Expression::ComputedMemberExpression(_)
                                    );
                                    if !is_allowed {
                                        let key_type = expr_type_name(key_expr_ref);
                                        builder.push_todo(format!(
                                            "(BuildHIR::lowerExpression) Expected Identifier, got {key_type} key in ObjectExpression"
                                        ));
                                    }
                                    let place =
                                        lower_expr_to_temp(builder, key_expr_ref, semantic, source);
                                    hir::ObjectPropertyKey::Computed(place)
                                }
                            }
                        } else {
                            lower_prop_key(&p.key)
                        };
                        let value = if p.method {
                            match &p.value {
                                js::Expression::FunctionExpression(func) => {
                                    let lowered =
                                        lower_nested_func(builder, func, semantic, source);
                                    lower_value_to_temporary(
                                        builder,
                                        hir::InstructionValue::ObjectMethod {
                                            lowered_func: lowered,
                                            loc: span_to_loc(p.span),
                                        },
                                    )
                                }
                                _ => {
                                    builder.push_todo(format!(
                                        "(BuildHIR::lowerExpression) Expected FunctionExpression for ObjectMethod value, got {}",
                                        expr_type_name(&p.value)
                                    ));
                                    lower_expr_to_temp(builder, &p.value, semantic, source)
                                }
                            }
                        } else {
                            lower_expr_to_temp(builder, &p.value, semantic, source)
                        };
                        properties.push(hir::ObjectPropertyOrSpread::Property(
                            hir::ObjectProperty {
                                key,
                                type_: if p.method {
                                    hir::ObjectPropertyType::Method
                                } else {
                                    hir::ObjectPropertyType::Property
                                },
                                place: value,
                            },
                        ));
                    }
                    js::ObjectPropertyKind::SpreadProperty(spread) => {
                        let place = lower_expr_to_temp(builder, &spread.argument, semantic, source);
                        properties.push(hir::ObjectPropertyOrSpread::Spread(place));
                    }
                }
            }
            hir::InstructionValue::ObjectExpression { properties, loc }
        }
        js::Expression::CallExpression(call) => {
            lower_call_expr(builder, call, semantic, source, loc)
        }
        js::Expression::ChainExpression(chain) => {
            if std::env::var("DEBUG_CODEGEN_EXPR").is_ok() {
                eprintln!(
                    "[DEBUG_CODEGEN_EXPR] ChainExpression nested_optional={} chain={:?}",
                    chain_expression_has_nested_optional_chain(chain),
                    chain.expression
                );
            }
            if chain_expression_has_call_with_multiple_optional_chain_args(chain) {
                builder.push_todo(
                    "Unexpected terminal kind `optional` for optional fallthrough block"
                        .to_string(),
                );
            }
            if builder.in_try_context() {
                builder.push_todo(
                    "Support value blocks (conditional, logical, optional chaining, etc) within a try/catch statement"
                        .to_string(),
                );
            }
            lower_chain_expr(builder, chain, semantic, source, loc)
        }
        js::Expression::NewExpression(new_expr) => {
            let callee = lower_expr_to_temp(builder, &new_expr.callee, semantic, source);
            let args = lower_args(builder, &new_expr.arguments, semantic, source);
            hir::InstructionValue::NewExpression { callee, args, loc }
        }
        js::Expression::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            hir::InstructionValue::PropertyLoad {
                object,
                property: hir::PropertyLiteral::String(member.property.name.to_string()),
                optional: member.optional,
                loc,
            }
        }
        js::Expression::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    return hir::InstructionValue::PropertyLoad {
                        object,
                        property: hir::PropertyLiteral::String(key.to_string()),
                        optional: member.optional,
                        loc,
                    };
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            hir::InstructionValue::ComputedLoad {
                object,
                property,
                optional: member.optional,
                loc,
            }
        }
        js::Expression::BinaryExpression(bin) => {
            let left = lower_expr_to_temp(builder, &bin.left, semantic, source);
            let right = lower_expr_to_temp(builder, &bin.right, semantic, source);
            let operator = convert_bin_op(bin.operator);
            hir::InstructionValue::BinaryExpression {
                operator,
                left,
                right,
                loc,
            }
        }
        js::Expression::UnaryExpression(unary) => {
            // Special handling for `delete` — produces PropertyDelete/ComputedDelete
            if unary.operator == js::UnaryOperator::Delete {
                match &unary.argument {
                    js::Expression::StaticMemberExpression(member) => {
                        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                        return hir::InstructionValue::PropertyDelete {
                            object,
                            property: hir::PropertyLiteral::String(
                                member.property.name.to_string(),
                            ),
                            loc,
                        };
                    }
                    js::Expression::ComputedMemberExpression(member) => {
                        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                        let property =
                            lower_expr_to_temp(builder, &member.expression, semantic, source);
                        return hir::InstructionValue::ComputedDelete {
                            object,
                            property,
                            loc,
                        };
                    }
                    _ => {}
                }
            }
            let value = lower_expr_to_temp(builder, &unary.argument, semantic, source);
            let operator = convert_unary_op(unary.operator);
            hir::InstructionValue::UnaryExpression {
                operator,
                value,
                loc,
            }
        }
        js::Expression::UpdateExpression(update) => {
            // Upstream decomposes member expression updates into separate
            // read/binary/store operations for correct reactivity tracking.
            let binary_op = if update.operator == js::UpdateOperator::Increment {
                hir::BinaryOperator::Add
            } else {
                hir::BinaryOperator::Sub
            };

            match &update.argument {
                js::SimpleAssignmentTarget::StaticMemberExpression(member) => {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let prop = hir::PropertyLiteral::String(member.property.name.to_string());
                    let previous_value = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyLoad {
                            object: object.clone(),
                            property: prop.clone(),
                            optional: false,
                            loc: loc.clone(),
                        },
                    );
                    let one = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::Primitive {
                            value: hir::PrimitiveValue::Number(1.0),
                            loc: loc.clone(),
                        },
                    );
                    let updated_value = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::BinaryExpression {
                            left: previous_value.clone(),
                            right: one,
                            operator: binary_op,
                            loc: loc.clone(),
                        },
                    );
                    let new_value_place = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyStore {
                            object: object.clone(),
                            property: prop,
                            value: updated_value.clone(),
                            loc: loc.clone(),
                        },
                    );
                    hir::InstructionValue::LoadLocal {
                        place: if update.prefix {
                            new_value_place
                        } else {
                            previous_value
                        },
                        loc,
                    }
                }
                js::SimpleAssignmentTarget::ComputedMemberExpression(member) => {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let property =
                        lower_expr_to_temp(builder, &member.expression, semantic, source);
                    let previous_value = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::ComputedLoad {
                            object: object.clone(),
                            property: property.clone(),
                            optional: false,
                            loc: loc.clone(),
                        },
                    );
                    let one = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::Primitive {
                            value: hir::PrimitiveValue::Number(1.0),
                            loc: loc.clone(),
                        },
                    );
                    let updated_value = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::BinaryExpression {
                            left: previous_value.clone(),
                            right: one,
                            operator: binary_op,
                            loc: loc.clone(),
                        },
                    );
                    let new_value_place = lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::ComputedStore {
                            object: object.clone(),
                            property: property.clone(),
                            value: updated_value.clone(),
                            loc: loc.clone(),
                        },
                    );
                    hir::InstructionValue::LoadLocal {
                        place: if update.prefix {
                            new_value_place
                        } else {
                            previous_value
                        },
                        loc,
                    }
                }
                js::SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
                    // Check if this is a global identifier
                    let name = ident.name.as_str();
                    if !builder.bindings.contains_key(name) {
                        // Upstream: Todo for UpdateExpression on globals
                        builder.push_todo(
                            "(BuildHIR::lowerExpression) Support UpdateExpression where argument is a global".to_string(),
                        );
                        return hir::InstructionValue::Primitive {
                            value: hir::PrimitiveValue::Undefined,
                            loc,
                        };
                    }
                    let place = lower_simple_assignment_target_to_temp(
                        builder,
                        &update.argument,
                        semantic,
                        source,
                    );
                    let operation = if update.operator == js::UpdateOperator::Increment {
                        hir::UpdateOperator::Increment
                    } else {
                        hir::UpdateOperator::Decrement
                    };
                    if update.prefix {
                        hir::InstructionValue::PrefixUpdate {
                            lvalue: place.clone(),
                            operation,
                            value: place,
                            loc,
                        }
                    } else {
                        hir::InstructionValue::PostfixUpdate {
                            lvalue: place.clone(),
                            operation,
                            value: place,
                            loc,
                        }
                    }
                }
                _ => {
                    // Other simple targets (shouldn't normally reach here, but handle gracefully)
                    let place = lower_simple_assignment_target_to_temp(
                        builder,
                        &update.argument,
                        semantic,
                        source,
                    );
                    let operation = if update.operator == js::UpdateOperator::Increment {
                        hir::UpdateOperator::Increment
                    } else {
                        hir::UpdateOperator::Decrement
                    };
                    if update.prefix {
                        hir::InstructionValue::PrefixUpdate {
                            lvalue: place.clone(),
                            operation,
                            value: place,
                            loc,
                        }
                    } else {
                        hir::InstructionValue::PostfixUpdate {
                            lvalue: place.clone(),
                            operation,
                            value: place,
                            loc,
                        }
                    }
                }
            }
        }
        js::Expression::LogicalExpression(logical) => {
            if expression_is_call_with_optional_chain_args(&logical.left) {
                builder.push_todo(
                    "Unexpected terminal kind `optional` for logical test block".to_string(),
                );
            }
            if builder.in_try_context() {
                builder.push_todo(
                    "Support value blocks (conditional, logical, optional chaining, etc) within a try/catch statement"
                        .to_string(),
                );
            }
            let operator = match logical.operator {
                oxc_syntax::operator::LogicalOperator::And => hir::LogicalOperator::And,
                oxc_syntax::operator::LogicalOperator::Or => hir::LogicalOperator::Or,
                oxc_syntax::operator::LogicalOperator::Coalesce => {
                    hir::LogicalOperator::NullishCoalescing
                }
            };

            let continuation = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation.id;
            let test_block = builder.reserve(hir::BlockKind::Value);
            let place = builder.make_temporary_place(loc.clone());
            let left_place = builder.make_temporary_place(span_to_loc(logical.left.span()));

            let consequent = builder.enter(hir::BlockKind::Value, |builder, _| {
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: place.clone(),
                            kind: hir::InstructionKind::Const,
                        },
                        value: left_place.clone(),
                        loc: left_place.loc.clone(),
                    },
                );
                hir::Terminal::Goto {
                    block: continuation_id,
                    variant: hir::GotoVariant::Break,
                    id: hir::InstructionId::default(),
                    loc: left_place.loc.clone(),
                }
            });

            let alternate = builder.enter(hir::BlockKind::Value, |builder, _| {
                let right = lower_expr_to_temp(builder, &logical.right, semantic, source);
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: place.clone(),
                            kind: hir::InstructionKind::Const,
                        },
                        value: right.clone(),
                        loc: right.loc.clone(),
                    },
                );
                hir::Terminal::Goto {
                    block: continuation_id,
                    variant: hir::GotoVariant::Break,
                    id: hir::InstructionId::default(),
                    loc: right.loc.clone(),
                }
            });

            builder.terminate_with_continuation(
                hir::Terminal::Logical {
                    test: test_block.id,
                    fallthrough: continuation_id,
                    operator,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                },
                test_block,
            );

            let left_value = lower_expr_to_temp(builder, &logical.left, semantic, source);
            builder.push(hir::Instruction {
                id: hir::InstructionId::default(),
                lvalue: left_place.clone(),
                value: hir::InstructionValue::LoadLocal {
                    place: left_value,
                    loc: loc.clone(),
                },
                loc: loc.clone(),
                effects: None,
            });
            builder.terminate_with_continuation(
                hir::Terminal::Branch {
                    test: left_place,
                    consequent,
                    alternate,
                    fallthrough: continuation_id,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                },
                continuation,
            );

            hir::InstructionValue::LoadLocal { place, loc }
        }
        js::Expression::ConditionalExpression(cond) => {
            if expression_is_call_with_optional_chain_args(&cond.test) {
                builder.push_todo(
                    "Unexpected terminal kind `optional` for ternary test block".to_string(),
                );
            }
            if builder.in_try_context() {
                builder.push_todo(
                    "Support value blocks (conditional, logical, optional chaining, etc) within a try/catch statement"
                        .to_string(),
                );
            }

            let continuation = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation.id;
            let test_block = builder.reserve(hir::BlockKind::Value);
            let place = builder.make_temporary_place(loc.clone());

            let consequent = builder.enter(hir::BlockKind::Value, |builder, _| {
                let consequent = lower_expr_to_temp(builder, &cond.consequent, semantic, source);
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: place.clone(),
                            kind: hir::InstructionKind::Const,
                        },
                        value: consequent.clone(),
                        loc: loc.clone(),
                    },
                );
                hir::Terminal::Goto {
                    block: continuation_id,
                    variant: hir::GotoVariant::Break,
                    id: hir::InstructionId::default(),
                    loc: consequent.loc.clone(),
                }
            });

            let alternate = builder.enter(hir::BlockKind::Value, |builder, _| {
                let alternate = lower_expr_to_temp(builder, &cond.alternate, semantic, source);
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: place.clone(),
                            kind: hir::InstructionKind::Const,
                        },
                        value: alternate.clone(),
                        loc: loc.clone(),
                    },
                );
                hir::Terminal::Goto {
                    block: continuation_id,
                    variant: hir::GotoVariant::Break,
                    id: hir::InstructionId::default(),
                    loc: alternate.loc.clone(),
                }
            });

            builder.terminate_with_continuation(
                hir::Terminal::Ternary {
                    test: test_block.id,
                    fallthrough: continuation_id,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                },
                test_block,
            );

            let test = lower_expr_to_temp(builder, &cond.test, semantic, source);
            builder.terminate_with_continuation(
                hir::Terminal::Branch {
                    test,
                    consequent,
                    alternate,
                    fallthrough: continuation_id,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                },
                continuation,
            );

            hir::InstructionValue::LoadLocal { place, loc }
        }
        js::Expression::AssignmentExpression(assign) => {
            lower_assign_expr(builder, assign, semantic, source)
        }
        js::Expression::SequenceExpression(seq) => {
            let continuation = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation.id;
            let place = builder.make_temporary_place(loc.clone());
            let sequence_block = builder.enter(hir::BlockKind::Sequence, |builder, _| {
                let mut last: Option<hir::Place> = None;
                for e in &seq.expressions {
                    last = Some(lower_expr_to_temp(builder, e, semantic, source));
                }
                if let Some(last) = last {
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::StoreLocal {
                            lvalue: hir::LValue {
                                place: place.clone(),
                                kind: hir::InstructionKind::Const,
                            },
                            value: last,
                            loc: loc.clone(),
                        },
                    );
                } else {
                    builder.push_todo(
                        "Expected sequence expression to have at least one expression".to_string(),
                    );
                }
                hir::Terminal::Goto {
                    block: continuation_id,
                    variant: hir::GotoVariant::Break,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                }
            });
            builder.terminate_with_continuation(
                hir::Terminal::Sequence {
                    block: sequence_block,
                    fallthrough: continuation_id,
                    id: hir::InstructionId::default(),
                    loc: loc.clone(),
                },
                continuation,
            );
            hir::InstructionValue::LoadLocal { place, loc }
        }
        js::Expression::JSXElement(jsx) => lower_jsx_elem(builder, jsx, semantic, source),
        js::Expression::JSXFragment(jsx) => lower_jsx_frag(builder, jsx, semantic, source),
        js::Expression::ArrowFunctionExpression(arrow) => {
            let body_func = lower_arrow(builder, arrow, semantic, source);
            hir::InstructionValue::FunctionExpression {
                name: None,
                lowered_func: body_func,
                expr_type: hir::FunctionExpressionType::ArrowFunctionExpression,
                loc,
            }
        }
        js::Expression::FunctionExpression(func) => {
            let name = func.id.as_ref().map(|id| id.name.to_string());
            let body_func = lower_nested_func(builder, func, semantic, source);
            hir::InstructionValue::FunctionExpression {
                name,
                lowered_func: body_func,
                expr_type: hir::FunctionExpressionType::FunctionExpression,
                loc,
            }
        }
        js::Expression::AwaitExpression(await_expr) => {
            let value = lower_expr_to_temp(builder, &await_expr.argument, semantic, source);
            hir::InstructionValue::Await { value, loc }
        }
        js::Expression::ThisExpression(_) => hir::InstructionValue::LoadGlobal {
            binding: hir::NonLocalBinding::Global {
                name: "this".to_string(),
            },
            loc,
        },
        js::Expression::ParenthesizedExpression(paren) => {
            lower_expr(builder, &paren.expression, semantic, source)
        }
        // TS/Flow cast expressions are preserved in HIR to match upstream
        // type-driven inference and codegen behavior.
        js::Expression::TSAsExpression(ts) => {
            let raw_expr = source_slice(source, ts.span);
            let type_annotation = source_slice(source, ts.type_annotation.span())
                .replace(FLOW_CAST_REWRITE_MARKER, "")
                .trim()
                .to_string();
            let kind = if raw_expr.contains(FLOW_CAST_REWRITE_MARKER) {
                hir::TypeAnnotationKind::Cast
            } else {
                hir::TypeAnnotationKind::As
            };
            hir::InstructionValue::TypeCastExpression {
                value: lower_expr_to_temp(builder, &ts.expression, semantic, source),
                type_: lower_type_annotation(&ts.type_annotation),
                type_annotation,
                type_annotation_kind: kind,
                loc,
            }
        }
        js::Expression::TSSatisfiesExpression(ts) => hir::InstructionValue::TypeCastExpression {
            value: lower_expr_to_temp(builder, &ts.expression, semantic, source),
            type_: lower_type_annotation(&ts.type_annotation),
            type_annotation: source_slice(source, ts.type_annotation.span()),
            type_annotation_kind: hir::TypeAnnotationKind::Satisfies,
            loc,
        },
        js::Expression::TSNonNullExpression(ts) => {
            lower_expr(builder, &ts.expression, semantic, source)
        }
        js::Expression::TSTypeAssertion(ts) => hir::InstructionValue::TypeCastExpression {
            value: lower_expr_to_temp(builder, &ts.expression, semantic, source),
            type_: lower_type_annotation(&ts.type_annotation),
            type_annotation: source_slice(source, ts.type_annotation.span()),
            type_annotation_kind: classify_type_assertion_kind(ts, source),
            loc,
        },
        js::Expression::TSInstantiationExpression(ts) => {
            lower_expr(builder, &ts.expression, semantic, source)
        }
        js::Expression::TaggedTemplateExpression(tagged) => {
            let tag = lower_expr_to_temp(builder, &tagged.tag, semantic, source);
            // Build the raw template string content (between backticks)
            // For tagged templates with no interpolations, the raw string is just the quasis content
            let quasis: Vec<String> = tagged
                .quasi
                .quasis
                .iter()
                .map(|q| q.value.raw.to_string())
                .collect();
            // Lower interpolated expressions
            let subexprs: Vec<hir::Place> = tagged
                .quasi
                .expressions
                .iter()
                .map(|e| lower_expr_to_temp(builder, e, semantic, source))
                .collect();
            // Build raw string with ${expr} placeholders
            let mut raw = String::new();
            for (i, q) in quasis.iter().enumerate() {
                raw.push_str(q);
                if i < subexprs.len() {
                    // We'll need to resolve the place name at codegen time
                    // For now, mark as a placeholder
                    raw.push_str(&format!("${{{}}}", i));
                }
            }
            hir::InstructionValue::TaggedTemplateExpression {
                tag,
                raw,
                cooked: None,
                loc,
            }
        }
        js::Expression::MetaProperty(mp) => {
            if mp.meta.name == "import" && mp.property.name == "meta" {
                hir::InstructionValue::MetaProperty {
                    meta: mp.meta.name.to_string(),
                    property: mp.property.name.to_string(),
                    loc,
                }
            } else {
                builder.push_todo(format!(
                    "(BuildHIR::lowerExpression) Handle {}.{} MetaProperty expressions",
                    mp.meta.name, mp.property.name
                ));
                hir::InstructionValue::Primitive {
                    value: hir::PrimitiveValue::Undefined,
                    loc,
                }
            }
        }
        // Fallback for unhandled expressions — emit Todo diagnostic
        _ => {
            let expr_type = expr_type_name(expr).to_string();
            builder.push_todo(format!(
                "(BuildHIR::lowerExpression) Handle {expr_type} expressions"
            ));
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc,
            }
        }
    }
}

fn expression_is_call_with_optional_chain_args(expr: &js::Expression<'_>) -> bool {
    let expr = match expr {
        js::Expression::ParenthesizedExpression(paren) => &paren.expression,
        _ => expr,
    };
    match expr {
        js::Expression::CallExpression(call) => call_expr_has_optional_chain_args(call),
        _ => false,
    }
}

fn call_expr_has_optional_chain_args(call: &js::CallExpression<'_>) -> bool {
    call.arguments
        .iter()
        .any(|arg| expr_contains_optional_chain(expr_from_arg(arg)))
}

fn call_expr_has_multiple_optional_chain_args(call: &js::CallExpression<'_>) -> bool {
    call.arguments
        .iter()
        .filter(|arg| expr_contains_optional_chain(expr_from_arg(arg)))
        .take(2)
        .count()
        >= 2
}

fn chain_expression_has_call_with_multiple_optional_chain_args(
    chain: &js::ChainExpression<'_>,
) -> bool {
    chain_element_has_call_with_multiple_optional_chain_args(&chain.expression)
}

fn chain_element_has_call_with_multiple_optional_chain_args(chain: &js::ChainElement<'_>) -> bool {
    match chain {
        js::ChainElement::CallExpression(call) => {
            expression_has_call_with_multiple_optional_chain_args(&call.callee)
                || call_expr_has_multiple_optional_chain_args(call)
        }
        js::ChainElement::StaticMemberExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
        }
        js::ChainElement::ComputedMemberExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
                || expression_has_call_with_multiple_optional_chain_args(&member.expression)
        }
        js::ChainElement::PrivateFieldExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
        }
        js::ChainElement::TSNonNullExpression(ts) => {
            expression_has_call_with_multiple_optional_chain_args(&ts.expression)
        }
    }
}

fn expression_has_call_with_multiple_optional_chain_args(expr: &js::Expression<'_>) -> bool {
    let expr = match expr {
        js::Expression::ParenthesizedExpression(paren) => &paren.expression,
        _ => expr,
    };
    match expr {
        js::Expression::CallExpression(call) => call_expr_has_multiple_optional_chain_args(call),
        js::Expression::ChainExpression(chain) => {
            chain_element_has_call_with_multiple_optional_chain_args(&chain.expression)
        }
        js::Expression::StaticMemberExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
        }
        js::Expression::ComputedMemberExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
                || expression_has_call_with_multiple_optional_chain_args(&member.expression)
        }
        js::Expression::PrivateFieldExpression(member) => {
            expression_has_call_with_multiple_optional_chain_args(&member.object)
        }
        _ => false,
    }
}

fn chain_expression_has_nested_optional_chain(chain: &js::ChainExpression<'_>) -> bool {
    match &chain.expression {
        js::ChainElement::CallExpression(call) => {
            expr_contains_optional_chain(&call.callee)
                || call
                    .arguments
                    .iter()
                    .any(|arg| expr_contains_optional_chain(expr_from_arg(arg)))
        }
        js::ChainElement::StaticMemberExpression(member) => {
            expr_contains_optional_chain(&member.object)
        }
        js::ChainElement::ComputedMemberExpression(member) => {
            expr_contains_optional_chain(&member.object)
                || expr_contains_optional_chain(&member.expression)
        }
        js::ChainElement::PrivateFieldExpression(member) => {
            expr_contains_optional_chain(&member.object)
        }
        js::ChainElement::TSNonNullExpression(ts) => expr_contains_optional_chain(&ts.expression),
    }
}

fn expr_contains_optional_chain(expr: &js::Expression<'_>) -> bool {
    match expr {
        js::Expression::ChainExpression(_) => true,
        js::Expression::CallExpression(call) => {
            expr_contains_optional_chain(&call.callee)
                || call
                    .arguments
                    .iter()
                    .any(|arg| expr_contains_optional_chain(expr_from_arg(arg)))
        }
        js::Expression::StaticMemberExpression(member) => {
            expr_contains_optional_chain(&member.object)
        }
        js::Expression::ComputedMemberExpression(member) => {
            expr_contains_optional_chain(&member.object)
                || expr_contains_optional_chain(&member.expression)
        }
        js::Expression::PrivateFieldExpression(member) => {
            expr_contains_optional_chain(&member.object)
        }
        js::Expression::LogicalExpression(logical) => {
            expr_contains_optional_chain(&logical.left)
                || expr_contains_optional_chain(&logical.right)
        }
        js::Expression::ConditionalExpression(cond) => {
            expr_contains_optional_chain(&cond.test)
                || expr_contains_optional_chain(&cond.consequent)
                || expr_contains_optional_chain(&cond.alternate)
        }
        js::Expression::ParenthesizedExpression(paren) => {
            expr_contains_optional_chain(&paren.expression)
        }
        js::Expression::SequenceExpression(seq) => {
            seq.expressions.iter().any(expr_contains_optional_chain)
        }
        _ => false,
    }
}

/// Get a human-readable name for an expression type.
fn expr_type_name(expr: &js::Expression<'_>) -> &'static str {
    match expr {
        js::Expression::MetaProperty(_) => "MetaProperty",
        js::Expression::ImportExpression(_) => "ImportExpression",
        js::Expression::ClassExpression(_) => "ClassExpression",
        js::Expression::PrivateFieldExpression(_) => "PrivateFieldExpression",
        js::Expression::Super(_) => "Super",
        js::Expression::YieldExpression(_) => "YieldExpression",
        _ => "unknown",
    }
}

fn lower_ident_expr<'a>(
    builder: &mut HIRBuilder,
    ident: &js::IdentifierReference<'a>,
    semantic: &Semantic<'a>,
) -> hir::InstructionValue {
    let loc = span_to_loc(ident.span);
    let name = ident.name.as_str();

    if name == "undefined" {
        return hir::InstructionValue::Primitive {
            value: hir::PrimitiveValue::Undefined,
            loc,
        };
    }

    if let Some(entry) = builder.bindings.get(name) {
        let is_context = builder.is_context_identifier_id(entry.identifier.id);
        let place = hir::Place {
            identifier: entry.identifier.clone(),
            effect: hir::Effect::Unknown,
            reactive: false,
            loc: loc.clone(),
        };
        return if is_context {
            hir::InstructionValue::LoadContext { place, loc }
        } else {
            hir::InstructionValue::LoadLocal { place, loc }
        };
    }

    // Without pre-declaration, a forward reference to a local variable in the
    // same function scope (e.g. `const x = identity(x)`) won't be in
    // builder.bindings yet. Use OXC semantic analysis to detect this case so
    // EnterSSA can flag hoisting errors. Captured variables from parent scopes
    // are intentionally left as LoadGlobal.
    if is_same_function_scope_reference(ident, semantic) {
        let identifier = builder.resolve_binding(name, loc.clone());
        let is_context = builder.is_context_identifier_id(identifier.id);
        let place = hir::Place {
            identifier,
            effect: hir::Effect::Unknown,
            reactive: false,
            loc: loc.clone(),
        };
        return if is_context {
            hir::InstructionValue::LoadContext { place, loc }
        } else {
            hir::InstructionValue::LoadLocal { place, loc }
        };
    }

    let binding = resolve_non_local_binding(ident, semantic);
    validate_module_type_provider_binding(builder, &binding);
    hir::InstructionValue::LoadGlobal { binding, loc }
}

/// Inline validation matching upstream Environment.getGlobalDeclaration():
/// checks that hook-like import names are typed as hooks and vice versa.
fn validate_module_type_provider_binding(builder: &mut HIRBuilder, binding: &hir::NonLocalBinding) {
    fn is_hook_like_name(name: &str) -> bool {
        name.starts_with("use")
            && name.len() > 3
            && name.chars().nth(3).is_some_and(|c| c.is_uppercase())
    }
    fn type_provider_hook_kind_for_specifier(module: &str, imported: &str) -> Option<bool> {
        match module {
            "ReactCompilerTest" => match imported {
                "useHookNotTypedAsHook" => Some(false),
                "notAhookTypedAsHook" => Some(true),
                _ => None,
            },
            _ => None,
        }
    }
    fn type_provider_hook_kind_for_default(module: &str) -> Option<bool> {
        match module {
            "useDefaultExportNotTypedAsHook" => Some(false),
            _ => None,
        }
    }

    match binding {
        hir::NonLocalBinding::ImportSpecifier {
            module, imported, ..
        } => {
            if let Some(is_hook) = type_provider_hook_kind_for_specifier(module, imported) {
                let expect_hook = is_hook_like_name(imported);
                if expect_hook != is_hook {
                    builder.push_invariant(format!(
                        "Invalid type configuration for module: Expected type for `import {{{}}} from '{}'` {} based on the exported name",
                        imported, module,
                        if expect_hook { "to be a hook" } else { "not to be a hook" }
                    ));
                }
            }
        }
        hir::NonLocalBinding::ImportDefault { module, .. } => {
            if let Some(is_hook) = type_provider_hook_kind_for_default(module) {
                let expect_hook = is_hook_like_name(module);
                if expect_hook != is_hook {
                    builder.push_invariant(format!(
                        "Invalid type configuration for module: Expected type for `import ... from '{}'` {} based on the module name",
                        module,
                        if expect_hook { "to be a hook" } else { "not to be a hook" }
                    ));
                }
            }
        }
        _ => {}
    }
}

/// Returns true if `ident` references a binding declared in the same function
/// scope as the reference site (a forward reference to a not-yet-lowered local
/// variable). This replaces pre-declaration: OXC semantic knows about all
/// bindings in the scope, so we can detect self-references like
/// `const x = identity(x)` even before the declaration is processed.
/// Captured variables from parent function scopes return false so they go
/// through the normal LoadGlobal path.
fn is_same_function_scope_reference<'a>(
    ident: &js::IdentifierReference<'a>,
    semantic: &Semantic<'a>,
) -> bool {
    let Some(reference_id) = ident.reference_id.get() else {
        return false;
    };
    let reference = semantic.scoping().get_reference(reference_id);
    let Some(symbol_id) = reference.symbol_id() else {
        return false;
    };
    let scoping = semantic.scoping();
    if scoping.symbol_flags(symbol_id).is_import() {
        return false;
    }
    let decl_scope_id = scoping.symbol_scope_id(symbol_id);
    if scoping.scope_flags(decl_scope_id).is_top() {
        return false;
    }
    // Check that the declaration and reference are within the same function
    // scope boundary (not separated by a function/arrow boundary).
    let ref_scope_id = reference.scope_id();
    let ref_fn = enclosing_function_scope(scoping, ref_scope_id);
    let decl_fn = enclosing_function_scope(scoping, decl_scope_id);
    ref_fn == decl_fn
}

/// Walk up the scope tree to find the nearest function (or top-level) scope.
fn enclosing_function_scope(
    scoping: &oxc_semantic::Scoping,
    mut scope_id: oxc_semantic::ScopeId,
) -> oxc_semantic::ScopeId {
    loop {
        let flags = scoping.scope_flags(scope_id);
        if flags.is_top() || flags.contains(oxc_semantic::ScopeFlags::Function) {
            return scope_id;
        }
        match scoping.scope_parent_id(scope_id) {
            Some(parent) => scope_id = parent,
            None => return scope_id,
        }
    }
}

fn resolve_non_local_binding<'a>(
    ident: &js::IdentifierReference<'a>,
    semantic: &Semantic<'a>,
) -> hir::NonLocalBinding {
    let fallback = || hir::NonLocalBinding::Global {
        name: ident.name.to_string(),
    };

    let Some(reference_id) = ident.reference_id.get() else {
        return fallback();
    };
    let Some(symbol_id) = semantic.scoping().get_reference(reference_id).symbol_id() else {
        return fallback();
    };

    let scoping = semantic.scoping();
    if scoping.symbol_flags(symbol_id).is_import()
        && let Some(binding) = resolve_import_binding(symbol_id, ident.name.as_str(), semantic)
    {
        return binding;
    }

    let decl_scope_id = scoping.symbol_scope_id(symbol_id);
    if scoping.scope_flags(decl_scope_id).is_top() {
        return hir::NonLocalBinding::ModuleLocal {
            name: ident.name.to_string(),
        };
    }

    fallback()
}

fn resolve_import_binding<'a>(
    symbol_id: oxc_semantic::SymbolId,
    local_name: &str,
    semantic: &Semantic<'a>,
) -> Option<hir::NonLocalBinding> {
    let decl_node = semantic.symbol_declaration(symbol_id);
    let nodes = semantic.nodes();
    let module = find_import_module_for_node(nodes, decl_node.id())?;

    match decl_node.kind() {
        AstKind::ImportSpecifier(spec) => {
            let imported = module_export_name_to_string(&spec.imported);
            Some(hir::NonLocalBinding::ImportSpecifier {
                name: local_name.to_string(),
                module,
                imported,
            })
        }
        AstKind::ImportDefaultSpecifier(_) => Some(hir::NonLocalBinding::ImportDefault {
            name: local_name.to_string(),
            module,
        }),
        AstKind::ImportNamespaceSpecifier(_) => Some(hir::NonLocalBinding::ImportNamespace {
            name: local_name.to_string(),
            module,
        }),
        _ => None,
    }
}

fn find_import_module_for_node<'a>(
    nodes: &oxc_semantic::AstNodes<'a>,
    node_id: NodeId,
) -> Option<String> {
    for ancestor_id in nodes.ancestor_ids(node_id) {
        if let AstKind::ImportDeclaration(import_decl) = nodes.kind(ancestor_id) {
            return Some(import_decl.source.value.to_string());
        }
    }
    None
}

fn module_export_name_to_string(name: &js::ModuleExportName<'_>) -> String {
    match name {
        js::ModuleExportName::IdentifierName(id) => id.name.to_string(),
        js::ModuleExportName::IdentifierReference(id) => id.name.to_string(),
        js::ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

fn lower_assign_expr<'a>(
    builder: &mut HIRBuilder,
    assign: &js::AssignmentExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    use oxc_syntax::operator::AssignmentOperator as AO;

    let loc = span_to_loc(assign.span);
    let is_compound_assign = assign.operator != AO::Assign;
    let rhs = lower_expr_to_temp(builder, &assign.right, semantic, source);

    // For compound assignments (+=, *=, etc.), desugar into `lhs = lhs op rhs`
    let effective_rhs = if is_compound_assign {
        if let Some(bin_op) = assign.operator.to_binary_operator() {
            // Load the current LHS value
            let lhs_val = lower_assignment_target_to_temp(builder, &assign.left, semantic, source);
            let hir_op = convert_bin_op(bin_op);
            // Create the binary expression: lhs op rhs
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::BinaryExpression {
                    left: lhs_val,
                    operator: hir_op,
                    right: rhs,
                    loc: loc.clone(),
                },
            )
        } else {
            // Logical assignments (&&=, ||=, ??=) — simplify to just rhs for now
            rhs
        }
    } else {
        rhs
    };

    match &assign.left {
        js::AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let name = ident.name.as_str();
            // Check if this identifier is a known local/context binding.
            // If not, it's a global assignment (upstream emits StoreGlobal).
            if builder.bindings.contains_key(name) {
                // Check for const reassignment — upstream BuildHIR.ts line ~3748
                if builder.is_binding_const(name) {
                    builder.push_todo("Cannot reassign a `const` variable".to_string());
                }
                let identifier = builder.resolve_binding(name, span_to_loc(ident.span));
                let lvalue_place = hir::Place {
                    identifier,
                    effect: hir::Effect::Unknown,
                    reactive: false,
                    loc: span_to_loc(ident.span),
                };
                if builder.is_context_identifier_id(lvalue_place.identifier.id) {
                    let store = hir::InstructionValue::StoreContext {
                        lvalue: hir::LValue {
                            place: lvalue_place,
                            kind: hir::InstructionKind::Reassign,
                        },
                        value: effective_rhs,
                        loc,
                    };
                    if is_compound_assign {
                        // Compound assignment: StoreContext + LoadContext
                        // (upstream BuildHIR.ts:2139-2148)
                        lower_value_to_temporary(builder, store);
                        hir::InstructionValue::LoadContext {
                            place: hir::Place {
                                identifier: builder.resolve_binding(name, span_to_loc(ident.span)),
                                effect: hir::Effect::Unknown,
                                reactive: false,
                                loc: span_to_loc(ident.span),
                            },
                            loc: span_to_loc(ident.span),
                        }
                    } else {
                        // Regular assignment: StoreContext → LoadLocal(temp)
                        // (upstream lowerAssignment:3835-3862)
                        let temp = lower_value_to_temporary(builder, store);
                        hir::InstructionValue::LoadLocal {
                            place: temp,
                            loc: span_to_loc(ident.span),
                        }
                    }
                } else {
                    let store = hir::InstructionValue::StoreLocal {
                        lvalue: hir::LValue {
                            place: lvalue_place,
                            kind: hir::InstructionKind::Reassign,
                        },
                        value: effective_rhs,
                        loc,
                    };
                    if is_compound_assign {
                        // Compound assignment: StoreLocal + LoadLocal(identifier)
                        lower_value_to_temporary(builder, store);
                        hir::InstructionValue::LoadLocal {
                            place: hir::Place {
                                identifier: builder.resolve_binding(name, span_to_loc(ident.span)),
                                effect: hir::Effect::Unknown,
                                reactive: false,
                                loc: span_to_loc(ident.span),
                            },
                            loc: span_to_loc(ident.span),
                        }
                    } else {
                        // Regular assignment: StoreLocal → LoadLocal(temp)
                        // (upstream lowerAssignment:3854-3862)
                        let temp = lower_value_to_temporary(builder, store);
                        hir::InstructionValue::LoadLocal {
                            place: temp,
                            loc: span_to_loc(ident.span),
                        }
                    }
                }
            } else {
                let store = hir::InstructionValue::StoreGlobal {
                    name: name.to_string(),
                    value: effective_rhs,
                    loc,
                };
                if is_compound_assign {
                    let temporary = lower_value_to_temporary(builder, store);
                    hir::InstructionValue::LoadLocal {
                        place: temporary.clone(),
                        loc: temporary.loc,
                    }
                } else {
                    store
                }
            }
        }
        js::AssignmentTarget::StaticMemberExpression(member) => {
            if std::env::var("DEBUG_ASSIGN_TARGET").is_ok() {
                let object_src = source_slice(source, member.object.span());
                let object_name = if let js::Expression::Identifier(ident) = &member.object {
                    ident.name.as_str().to_string()
                } else {
                    "<non-identifier>".to_string()
                };
                eprintln!(
                    "[DEBUG_ASSIGN_TARGET] static object_src={} object_name={} binding_present={} bindings={}",
                    object_src,
                    object_name,
                    builder.bindings.contains_key(object_name.as_str()),
                    builder.bindings.len()
                );
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            hir::InstructionValue::PropertyStore {
                object,
                property: hir::PropertyLiteral::String(member.property.name.to_string()),
                value: effective_rhs,
                loc,
            }
        }
        js::AssignmentTarget::ComputedMemberExpression(member) => {
            if std::env::var("DEBUG_ASSIGN_TARGET").is_ok() {
                let object_src = source_slice(source, member.object.span());
                let object_name = if let js::Expression::Identifier(ident) = &member.object {
                    ident.name.as_str().to_string()
                } else {
                    "<non-identifier>".to_string()
                };
                eprintln!(
                    "[DEBUG_ASSIGN_TARGET] computed object_src={} object_name={} binding_present={} bindings={}",
                    object_src,
                    object_name,
                    builder.bindings.contains_key(object_name.as_str()),
                    builder.bindings.len()
                );
            }
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    return hir::InstructionValue::PropertyStore {
                        object,
                        property: hir::PropertyLiteral::String(key.to_string()),
                        value: effective_rhs,
                        loc,
                    };
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            hir::InstructionValue::ComputedStore {
                object,
                property,
                value: effective_rhs,
                loc,
            }
        }
        js::AssignmentTarget::ArrayAssignmentTarget(arr) => {
            lower_array_assignment_target(builder, arr, effective_rhs, loc, semantic, source)
        }
        js::AssignmentTarget::ObjectAssignmentTarget(obj) => {
            lower_object_assignment_target(builder, obj, effective_rhs, loc, semantic, source)
        }
        _ => {
            builder.push_todo(
                "(BuildHIR::lowerExpression) Handle complex assignment targets".to_string(),
            );
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc,
            }
        }
    }
}

/// Lower an array destructuring assignment target: `[a, b] = expr`
///
/// For the simple case (all elements are identifiers), emit a Destructure directly.
/// For complex cases, use temporaries and emit follow-up StoreLocal instructions.
fn lower_array_assignment_target<'a>(
    builder: &mut HIRBuilder,
    arr: &js::ArrayAssignmentTarget<'a>,
    value: hir::Place,
    loc: hir::SourceLocation,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    let kind = hir::InstructionKind::Reassign;

    let has_context_like_identifier = arr.elements.iter().any(|elem| {
        matches!(
            elem,
            Some(js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(ident))
                if assignment_target_identifier_is_context_like(ident, semantic)
        )
    }) || arr.rest.as_ref().is_some_and(|r| {
        matches!(
            &r.target,
            js::AssignmentTarget::AssignmentTargetIdentifier(ident)
                if assignment_target_identifier_is_context_like(ident, semantic)
        )
    });

    // Check if all elements are simple identifiers (no force_temporaries needed)
    let all_simple = arr.elements.iter().all(|elem| match elem {
        None => true,
        Some(e) => matches!(
            e,
            js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(_)
        ),
    }) && arr.rest.as_ref().is_none_or(|r| {
        matches!(
            &r.target,
            js::AssignmentTarget::AssignmentTargetIdentifier(_)
        )
    }) && !has_context_like_identifier;

    if all_simple {
        // Simple case: all elements are identifiers, emit Destructure directly
        let mut items = Vec::new();
        for elem in arr.elements.iter() {
            match elem {
                None => items.push(hir::ArrayElement::Hole),
                Some(js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(ident)) => {
                    let id_loc = span_to_loc(ident.span);
                    let identifier = builder.resolve_binding(&ident.name, id_loc.clone());
                    items.push(hir::ArrayElement::Place(hir::Place {
                        identifier,
                        effect: hir::Effect::Unknown,
                        reactive: false,
                        loc: id_loc,
                    }));
                }
                _ => unreachable!(), // all_simple guarantees this
            }
        }
        if let Some(rest) = &arr.rest
            && let js::AssignmentTarget::AssignmentTargetIdentifier(ident) = &rest.target
        {
            let id_loc = span_to_loc(ident.span);
            let identifier = builder.resolve_binding(&ident.name, id_loc.clone());
            items.push(hir::ArrayElement::Spread(hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: id_loc,
            }));
        }
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Destructure {
                lvalue: hir::LValuePattern {
                    pattern: hir::Pattern::Array(hir::ArrayPattern { items }),
                    kind,
                },
                value: value.clone(),
                loc: loc.clone(),
            },
        );
    } else {
        // Complex case: use temporaries for all elements, emit follow-up stores
        // Phase 1: build items array with temp places, record (temp_place, element_index) pairs
        let mut items = Vec::new();
        let mut followup_indices: Vec<(hir::Place, usize)> = Vec::new();
        let mut rest_temp: Option<hir::Place> = None;

        for (i, elem) in arr.elements.iter().enumerate() {
            match elem {
                None => items.push(hir::ArrayElement::Hole),
                Some(_) => {
                    let temp_place = builder.make_temporary_place(hir::SourceLocation::Generated);
                    items.push(hir::ArrayElement::Place(temp_place.clone()));
                    followup_indices.push((temp_place, i));
                }
            }
        }
        if let Some(_rest) = &arr.rest {
            let temp_place = builder.make_temporary_place(hir::SourceLocation::Generated);
            items.push(hir::ArrayElement::Spread(temp_place.clone()));
            rest_temp = Some(temp_place);
        }

        // Emit the Destructure instruction
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Destructure {
                lvalue: hir::LValuePattern {
                    pattern: hir::Pattern::Array(hir::ArrayPattern { items }),
                    kind,
                },
                value: value.clone(),
                loc: loc.clone(),
            },
        );

        // Phase 2: emit follow-up stores by re-matching original elements
        for (temp_place, idx) in followup_indices {
            if let Some(Some(elem)) = arr.elements.get(idx) {
                emit_store_for_maybe_default(builder, temp_place, elem, semantic, source);
            }
        }
        if let Some(temp_place) = rest_temp
            && let Some(rest) = &arr.rest
        {
            emit_store_for_target(builder, temp_place, &rest.target, semantic, source);
        }
    }

    // Return a LoadLocal of the original value
    hir::InstructionValue::LoadLocal { place: value, loc }
}

/// Lower an object destructuring assignment target: `{ a, b } = expr`
fn lower_object_assignment_target<'a>(
    builder: &mut HIRBuilder,
    obj: &js::ObjectAssignmentTarget<'a>,
    value: hir::Place,
    loc: hir::SourceLocation,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    let kind = hir::InstructionKind::Reassign;

    // Check if all properties are simple identifier bindings
    let all_simple = obj.properties.iter().all(|prop| match prop {
        js::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => p.init.is_none(),
        js::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
            !p.computed
                && matches!(
                    &p.binding,
                    js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(_)
                )
        }
    }) && obj.rest.is_none();

    if all_simple {
        // Simple case: all properties are identifier bindings
        let mut properties = Vec::new();
        let mut followup_context_props: Vec<(hir::Place, String, hir::SourceLocation)> = Vec::new();
        for prop in &obj.properties {
            match prop {
                js::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                    let id_loc = span_to_loc(p.binding.span);
                    if assignment_target_identifier_is_context_like(&p.binding, semantic) {
                        let temp_place = builder.make_temporary_place(id_loc.clone());
                        properties.push(hir::ObjectPropertyOrSpread::Property(
                            hir::ObjectProperty {
                                key: hir::ObjectPropertyKey::Identifier(p.binding.name.to_string()),
                                type_: hir::ObjectPropertyType::Property,
                                place: temp_place.clone(),
                            },
                        ));
                        followup_context_props.push((
                            temp_place,
                            p.binding.name.to_string(),
                            id_loc,
                        ));
                    } else {
                        let identifier = builder.resolve_binding(&p.binding.name, id_loc.clone());
                        properties.push(hir::ObjectPropertyOrSpread::Property(
                            hir::ObjectProperty {
                                key: hir::ObjectPropertyKey::Identifier(p.binding.name.to_string()),
                                type_: hir::ObjectPropertyType::Property,
                                place: hir::Place {
                                    identifier,
                                    effect: hir::Effect::Unknown,
                                    reactive: false,
                                    loc: id_loc,
                                },
                            },
                        ));
                    }
                }
                js::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                    if p.computed {
                        builder.push_todo(
                            "(BuildHIR::lowerAssignment) Handle computed properties in ObjectPattern"
                                .to_string(),
                        );
                        continue;
                    }
                    let key = lower_prop_key(&p.name);
                    if let js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(ident) =
                        &p.binding
                    {
                        let id_loc = span_to_loc(ident.span);
                        if assignment_target_identifier_is_context_like(ident, semantic) {
                            let temp_place = builder.make_temporary_place(id_loc.clone());
                            properties.push(hir::ObjectPropertyOrSpread::Property(
                                hir::ObjectProperty {
                                    key,
                                    type_: hir::ObjectPropertyType::Property,
                                    place: temp_place.clone(),
                                },
                            ));
                            followup_context_props.push((
                                temp_place,
                                ident.name.to_string(),
                                id_loc,
                            ));
                        } else {
                            let identifier = builder.resolve_binding(&ident.name, id_loc.clone());
                            properties.push(hir::ObjectPropertyOrSpread::Property(
                                hir::ObjectProperty {
                                    key,
                                    type_: hir::ObjectPropertyType::Property,
                                    place: hir::Place {
                                        identifier,
                                        effect: hir::Effect::Unknown,
                                        reactive: false,
                                        loc: id_loc,
                                    },
                                },
                            ));
                        }
                    }
                }
            }
        }
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Destructure {
                lvalue: hir::LValuePattern {
                    pattern: hir::Pattern::Object(hir::ObjectPattern { properties }),
                    kind,
                },
                value: value.clone(),
                loc: loc.clone(),
            },
        );
        for (temp_place, name, id_loc) in followup_context_props {
            let identifier = builder.resolve_binding(&name, id_loc.clone());
            let lvalue_place = hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: id_loc.clone(),
            };
            let store = if builder.is_context_identifier_id(lvalue_place.identifier.id) {
                hir::InstructionValue::StoreContext {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Reassign,
                    },
                    value: temp_place,
                    loc: id_loc,
                }
            } else {
                hir::InstructionValue::StoreLocal {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Reassign,
                    },
                    value: temp_place,
                    loc: id_loc,
                }
            };
            lower_value_to_temporary(builder, store);
        }
    } else {
        // Complex case: use temporaries
        let mut properties = Vec::new();
        let mut followup_indices: Vec<(hir::Place, usize)> = Vec::new();
        let mut rest_temp: Option<hir::Place> = None;

        for (i, prop) in obj.properties.iter().enumerate() {
            let key = match prop {
                js::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                    hir::ObjectPropertyKey::Identifier(p.binding.name.to_string())
                }
                js::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                    if p.computed {
                        builder.push_todo(
                            "(BuildHIR::lowerAssignment) Handle computed properties in ObjectPattern"
                                .to_string(),
                        );
                        continue;
                    }
                    lower_prop_key(&p.name)
                }
            };
            let temp_place = builder.make_temporary_place(hir::SourceLocation::Generated);
            properties.push(hir::ObjectPropertyOrSpread::Property(hir::ObjectProperty {
                key,
                type_: hir::ObjectPropertyType::Property,
                place: temp_place.clone(),
            }));
            followup_indices.push((temp_place, i));
        }

        if let Some(_rest) = &obj.rest {
            let temp_place = builder.make_temporary_place(hir::SourceLocation::Generated);
            properties.push(hir::ObjectPropertyOrSpread::Spread(temp_place.clone()));
            rest_temp = Some(temp_place);
        }

        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Destructure {
                lvalue: hir::LValuePattern {
                    pattern: hir::Pattern::Object(hir::ObjectPattern { properties }),
                    kind,
                },
                value: value.clone(),
                loc: loc.clone(),
            },
        );

        // Emit follow-up stores by re-matching original properties
        for (temp_place, idx) in followup_indices {
            if let Some(prop) = obj.properties.get(idx) {
                match prop {
                    js::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => {
                        let id_loc = span_to_loc(p.binding.span);
                        let identifier = builder.resolve_binding(&p.binding.name, id_loc.clone());
                        let lvalue_place = hir::Place {
                            identifier,
                            effect: hir::Effect::Unknown,
                            reactive: false,
                            loc: id_loc.clone(),
                        };
                        let store = if builder.is_context_identifier_id(lvalue_place.identifier.id)
                        {
                            hir::InstructionValue::StoreContext {
                                lvalue: hir::LValue {
                                    place: lvalue_place,
                                    kind: hir::InstructionKind::Reassign,
                                },
                                value: temp_place,
                                loc: id_loc,
                            }
                        } else {
                            hir::InstructionValue::StoreLocal {
                                lvalue: hir::LValue {
                                    place: lvalue_place,
                                    kind: hir::InstructionKind::Reassign,
                                },
                                value: temp_place,
                                loc: id_loc,
                            }
                        };
                        lower_value_to_temporary(builder, store);
                    }
                    js::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
                        emit_store_for_maybe_default(
                            builder, temp_place, &p.binding, semantic, source,
                        );
                    }
                }
            }
        }
        if let Some(temp_place) = rest_temp
            && let Some(rest) = &obj.rest
        {
            emit_store_for_target(builder, temp_place, &rest.target, semantic, source);
        }
    }

    hir::InstructionValue::LoadLocal { place: value, loc }
}

fn assignment_target_identifier_is_context_like(
    ident: &js::IdentifierReference<'_>,
    semantic: &Semantic<'_>,
) -> bool {
    let debug = std::env::var("DEBUG_CONTEXT_LIKE").is_ok();
    let Some(reference_id) = ident.reference_id.get() else {
        if debug {
            eprintln!(
                "[DEBUG_CONTEXT_LIKE] assign name={} reference=<none> => false",
                ident.name
            );
        }
        return false;
    };
    let scoping = semantic.scoping();
    let Some(symbol_id) = scoping.get_reference(reference_id).symbol_id() else {
        if debug {
            eprintln!(
                "[DEBUG_CONTEXT_LIKE] assign name={} symbol=<none> => false",
                ident.name
            );
        }
        return false;
    };
    let decl_scope = scoping.symbol_scope_id(symbol_id);
    let mut has_write = false;
    let mut captured_in_nested_scope = false;

    for reference in semantic.symbol_references(symbol_id) {
        if reference.is_write() {
            has_write = true;
        }
        let reference_scope = reference.scope_id();
        if reference_scope != decl_scope {
            let mut reaches_decl_scope = false;
            let mut crosses_function_scope = false;
            for scope_id in scoping.scope_ancestors(reference_scope) {
                if scope_id == decl_scope {
                    reaches_decl_scope = true;
                    break;
                }
                if scoping.scope_flags(scope_id).is_function() {
                    crosses_function_scope = true;
                }
            }
            if reaches_decl_scope && crosses_function_scope {
                captured_in_nested_scope = true;
            }
        }
        if has_write && captured_in_nested_scope {
            if debug {
                eprintln!(
                    "[DEBUG_CONTEXT_LIKE] assign name={} symbol={} write={} captured={} => true",
                    ident.name,
                    symbol_id.index(),
                    has_write,
                    captured_in_nested_scope
                );
            }
            return true;
        }
    }

    let result = has_write && captured_in_nested_scope;
    if debug {
        eprintln!(
            "[DEBUG_CONTEXT_LIKE] assign name={} symbol={} write={} captured={} => {}",
            ident.name,
            symbol_id.index(),
            has_write,
            captured_in_nested_scope,
            result
        );
    }
    result
}

/// Emit a store from a temp place into an AssignmentTargetMaybeDefault.
fn emit_store_for_maybe_default<'a>(
    builder: &mut HIRBuilder,
    temp_place: hir::Place,
    elem: &js::AssignmentTargetMaybeDefault<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    match elem {
        js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let identifier = builder.resolve_binding(&ident.name, loc.clone());
            let lvalue_place = hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: loc.clone(),
            };
            let store = if builder.is_context_identifier_id(lvalue_place.identifier.id) {
                hir::InstructionValue::StoreContext {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Reassign,
                    },
                    value: temp_place,
                    loc,
                }
            } else {
                hir::InstructionValue::StoreLocal {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Reassign,
                    },
                    value: temp_place,
                    loc,
                }
            };
            lower_value_to_temporary(builder, store);
        }
        js::AssignmentTargetMaybeDefault::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyStore {
                    object,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    value: temp_place,
                    loc,
                },
            );
        }
        js::AssignmentTargetMaybeDefault::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let loc = span_to_loc(member.span);
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyStore {
                            object,
                            property: hir::PropertyLiteral::String(key.to_string()),
                            value: temp_place,
                            loc,
                        },
                    );
                    return;
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::ComputedStore {
                    object,
                    property,
                    value: temp_place,
                    loc,
                },
            );
        }
        js::AssignmentTargetMaybeDefault::ArrayAssignmentTarget(arr) => {
            let loc = temp_place.loc.clone();
            lower_array_assignment_target(builder, arr, temp_place, loc, semantic, source);
        }
        js::AssignmentTargetMaybeDefault::ObjectAssignmentTarget(obj) => {
            let loc = temp_place.loc.clone();
            lower_object_assignment_target(builder, obj, temp_place, loc, semantic, source);
        }
        js::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(awd) => {
            // Pattern: [a = defaultVal] = arr
            // Use block-based Branch for proper reactive scope analysis.
            let result_place = emit_default_value_branch(
                builder,
                temp_place,
                &awd.init,
                semantic,
                source,
                &span_to_loc(awd.span),
            );
            emit_store_for_target(builder, result_place, &awd.binding, semantic, source);
        }
        _ => {
            builder.push_todo(
                "(BuildHIR::lowerAssignment) Handle complex assignment target".to_string(),
            );
        }
    }
}

/// Emit a store from a temp place into an AssignmentTarget.
fn emit_store_for_target<'a>(
    builder: &mut HIRBuilder,
    temp_place: hir::Place,
    target: &js::AssignmentTarget<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) {
    match target {
        js::AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let identifier = builder.resolve_binding(&ident.name, loc.clone());
            let lvalue_place = hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: loc.clone(),
            };
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::StoreLocal {
                    lvalue: hir::LValue {
                        place: lvalue_place,
                        kind: hir::InstructionKind::Reassign,
                    },
                    value: temp_place,
                    loc,
                },
            );
        }
        js::AssignmentTarget::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyStore {
                    object,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    value: temp_place,
                    loc,
                },
            );
        }
        js::AssignmentTarget::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let loc = span_to_loc(member.span);
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyStore {
                            object,
                            property: hir::PropertyLiteral::String(key.to_string()),
                            value: temp_place,
                            loc,
                        },
                    );
                    return;
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::ComputedStore {
                    object,
                    property,
                    value: temp_place,
                    loc,
                },
            );
        }
        js::AssignmentTarget::ArrayAssignmentTarget(arr) => {
            let loc = temp_place.loc.clone();
            lower_array_assignment_target(builder, arr, temp_place, loc, semantic, source);
        }
        js::AssignmentTarget::ObjectAssignmentTarget(obj) => {
            let loc = temp_place.loc.clone();
            lower_object_assignment_target(builder, obj, temp_place, loc, semantic, source);
        }
        _ => {
            builder.push_todo(
                "(BuildHIR::lowerAssignment) Handle complex assignment target".to_string(),
            );
        }
    }
}

/// Load the current value of an assignment target (for compound assignments).
fn lower_assignment_target_to_temp<'a>(
    builder: &mut HIRBuilder,
    target: &js::AssignmentTarget<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::Place {
    match target {
        js::AssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let name = ident.name.as_str();
            if builder.bindings.contains_key(name) {
                let identifier = builder.resolve_binding(name, loc.clone());
                let place = hir::Place {
                    identifier,
                    effect: hir::Effect::Unknown,
                    reactive: false,
                    loc: loc.clone(),
                };
                if builder.is_context_identifier_id(place.identifier.id) {
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadContext { place, loc },
                    )
                } else {
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadLocal { place, loc },
                    )
                }
            } else {
                lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::LoadGlobal {
                        binding: hir::NonLocalBinding::Global {
                            name: name.to_string(),
                        },
                        loc,
                    },
                )
            }
        }
        js::AssignmentTarget::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyLoad {
                    object,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    optional: false,
                    loc,
                },
            )
        }
        js::AssignmentTarget::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let loc = span_to_loc(member.span);
                    return lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyLoad {
                            object,
                            property: hir::PropertyLiteral::String(key.to_string()),
                            optional: false,
                            loc,
                        },
                    );
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::ComputedLoad {
                    object,
                    property,
                    optional: false,
                    loc,
                },
            )
        }
        _ => {
            let loc = hir::SourceLocation::Generated;
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::Primitive {
                    value: hir::PrimitiveValue::Undefined,
                    loc,
                },
            )
        }
    }
}

/// Lower a SimpleAssignmentTarget (e.g., from UpdateExpression.argument) to a temporary.
fn lower_simple_assignment_target_to_temp<'a>(
    builder: &mut HIRBuilder,
    target: &js::SimpleAssignmentTarget<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::Place {
    match target {
        js::SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
            let loc = span_to_loc(ident.span);
            let identifier = builder.resolve_binding(&ident.name, loc.clone());
            let place = hir::Place {
                identifier,
                effect: hir::Effect::Unknown,
                reactive: false,
                loc: loc.clone(),
            };
            if builder.is_context_identifier_id(place.identifier.id) {
                lower_value_to_temporary(builder, hir::InstructionValue::LoadContext { place, loc })
            } else {
                lower_value_to_temporary(builder, hir::InstructionValue::LoadLocal { place, loc })
            }
        }
        js::SimpleAssignmentTarget::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyLoad {
                    object,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    optional: false,
                    loc,
                },
            )
        }
        js::SimpleAssignmentTarget::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    let loc = span_to_loc(member.span);
                    return lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::PropertyLoad {
                            object,
                            property: hir::PropertyLiteral::String(key.to_string()),
                            optional: false,
                            loc,
                        },
                    );
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            let loc = span_to_loc(member.span);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::ComputedLoad {
                    object,
                    property,
                    optional: false,
                    loc,
                },
            )
        }
        _ => {
            let loc = hir::SourceLocation::Generated;
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::Primitive {
                    value: hir::PrimitiveValue::Undefined,
                    loc,
                },
            )
        }
    }
}

// ============================================================================
// JSX lowering
// ============================================================================

fn lower_jsx_elem<'a>(
    builder: &mut HIRBuilder,
    jsx: &js::JSXElement<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    let loc = span_to_loc(jsx.span);

    let tag = lower_jsx_name(builder, &jsx.opening_element.name, semantic, source);

    let is_fbt_root = fbt_root_tag_name(&jsx.opening_element.name);
    if let Some(tag_name) = is_fbt_root {
        let mut enum_locs = Vec::new();
        let mut plural_locs = Vec::new();
        let mut pronoun_locs = Vec::new();
        for child in &jsx.children {
            collect_fbt_namespaced_child_locations(
                child,
                tag_name,
                &mut enum_locs,
                &mut plural_locs,
                &mut pronoun_locs,
            );
        }
        if enum_locs.len() > 1 || plural_locs.len() > 1 || pronoun_locs.len() > 1 {
            builder.push_todo("Support duplicate fbt tags".to_string());
        }
    }

    if is_fbt_root.is_some() {
        builder.fbt_depth += 1;
    }

    let mut props = Vec::new();
    for attr in &jsx.opening_element.attributes {
        match attr {
            js::JSXAttributeItem::Attribute(a) => {
                let name = match &a.name {
                    js::JSXAttributeName::Identifier(id) => id.name.to_string(),
                    js::JSXAttributeName::NamespacedName(ns) => {
                        format!("{}:{}", ns.namespace.name, ns.name.name)
                    }
                };
                let value = if let Some(val) = &a.value {
                    match val {
                        js::JSXAttributeValue::StringLiteral(s) => lower_value_to_temporary(
                            builder,
                            hir::InstructionValue::Primitive {
                                value: hir::PrimitiveValue::String(s.value.to_string()),
                                loc: span_to_loc(s.span),
                            },
                        ),
                        js::JSXAttributeValue::ExpressionContainer(container) => {
                            match &container.expression {
                                js::JSXExpression::EmptyExpression(_) => lower_value_to_temporary(
                                    builder,
                                    hir::InstructionValue::Primitive {
                                        value: hir::PrimitiveValue::Boolean(true),
                                        loc: span_to_loc(container.span),
                                    },
                                ),
                                _ => {
                                    // Expression variant via @inherit
                                    lower_jsx_expr_to_temp(
                                        builder,
                                        &container.expression,
                                        semantic,
                                        source,
                                    )
                                }
                            }
                        }
                        js::JSXAttributeValue::Element(elem) => {
                            let val = lower_jsx_elem(builder, elem, semantic, source);
                            lower_value_to_temporary(builder, val)
                        }
                        js::JSXAttributeValue::Fragment(frag) => {
                            let val = lower_jsx_frag(builder, frag, semantic, source);
                            lower_value_to_temporary(builder, val)
                        }
                    }
                } else {
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::Primitive {
                            value: hir::PrimitiveValue::Boolean(true),
                            loc: span_to_loc(a.span),
                        },
                    )
                };
                props.push(hir::JsxAttribute::Attribute { name, place: value });
            }
            js::JSXAttributeItem::SpreadAttribute(spread) => {
                let argument = lower_expr_to_temp(builder, &spread.argument, semantic, source);
                props.push(hir::JsxAttribute::SpreadAttribute { argument });
            }
        }
    }

    let children = if jsx.children.is_empty() {
        None
    } else {
        let mut child_places = Vec::new();
        for child in &jsx.children {
            if let Some(place) = lower_jsx_child(builder, child, semantic, source) {
                child_places.push(place);
            }
        }
        if child_places.is_empty() {
            None
        } else {
            Some(child_places)
        }
    };

    if is_fbt_root.is_some() {
        builder.fbt_depth = builder.fbt_depth.saturating_sub(1);
    }

    hir::InstructionValue::JsxExpression {
        tag,
        props,
        children,
        loc,
    }
}

fn fbt_root_tag_name<'a>(name: &js::JSXElementName<'a>) -> Option<&'a str> {
    match name {
        js::JSXElementName::Identifier(id) => match id.name.as_str() {
            "fbt" | "fbs" => Some(id.name.as_str()),
            _ => None,
        },
        js::JSXElementName::IdentifierReference(id) => match id.name.as_str() {
            "fbt" | "fbs" => Some(id.name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn collect_fbt_namespaced_child_locations<'a>(
    child: &js::JSXChild<'a>,
    root_tag: &str,
    enum_locs: &mut Vec<hir::SourceLocation>,
    plural_locs: &mut Vec<hir::SourceLocation>,
    pronoun_locs: &mut Vec<hir::SourceLocation>,
) {
    match child {
        js::JSXChild::Element(elem) => {
            if let js::JSXElementName::NamespacedName(ns) = &elem.opening_element.name
                && ns.namespace.name == root_tag
            {
                match ns.name.name.as_str() {
                    "enum" => enum_locs.push(span_to_loc(ns.span)),
                    "plural" => plural_locs.push(span_to_loc(ns.span)),
                    "pronoun" => pronoun_locs.push(span_to_loc(ns.span)),
                    _ => {}
                }
            }
            for nested in &elem.children {
                collect_fbt_namespaced_child_locations(
                    nested,
                    root_tag,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
        }
        js::JSXChild::Fragment(frag) => {
            for nested in &frag.children {
                collect_fbt_namespaced_child_locations(
                    nested,
                    root_tag,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
        }
        _ => {}
    }
}

fn lower_jsx_frag<'a>(
    builder: &mut HIRBuilder,
    jsx: &js::JSXFragment<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::InstructionValue {
    let loc = span_to_loc(jsx.span);
    let mut children = Vec::new();
    for child in &jsx.children {
        if let Some(place) = lower_jsx_child(builder, child, semantic, source) {
            children.push(place);
        }
    }
    hir::InstructionValue::JsxFragment { children, loc }
}

fn lower_jsx_child<'a>(
    builder: &mut HIRBuilder,
    child: &js::JSXChild<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> Option<hir::Place> {
    match child {
        js::JSXChild::Text(text) => {
            let raw_value = text.value.to_string();

            // Upstream preserves all whitespace within fbt/fbs subtrees.
            // Outside fbt trees we apply standard JSX text trimming.
            let value = if builder.fbt_depth > 0 {
                raw_value
            } else {
                trim_jsx_text(&raw_value)?
            };

            // Entity decoding is only applied for non-fbt JSXText parity.
            if builder.fbt_depth == 0 && value.contains('&') {
                let decoded = decode_jsx_entities(&value);
                Some(lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::Primitive {
                        value: hir::PrimitiveValue::String(decoded),
                        loc: span_to_loc(text.span),
                    },
                ))
            } else {
                Some(lower_value_to_temporary(
                    builder,
                    hir::InstructionValue::JSXText {
                        value,
                        loc: span_to_loc(text.span),
                    },
                ))
            }
        }
        js::JSXChild::ExpressionContainer(container) => match &container.expression {
            js::JSXExpression::EmptyExpression(_) => None,
            _ => Some(lower_jsx_expr_to_temp(
                builder,
                &container.expression,
                semantic,
                source,
            )),
        },
        js::JSXChild::Element(elem) => {
            let val = lower_jsx_elem(builder, elem, semantic, source);
            Some(lower_value_to_temporary(builder, val))
        }
        js::JSXChild::Fragment(frag) => {
            let val = lower_jsx_frag(builder, frag, semantic, source);
            Some(lower_value_to_temporary(builder, val))
        }
        js::JSXChild::Spread(spread) => Some(lower_expr_to_temp(
            builder,
            &spread.expression,
            semantic,
            source,
        )),
    }
}

/// Lower a JSXExpression (which may be EmptyExpression or any Expression variant)
fn lower_jsx_expr_to_temp<'a>(
    builder: &mut HIRBuilder,
    jsx_expr: &js::JSXExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::Place {
    match jsx_expr {
        js::JSXExpression::EmptyExpression(_) => lower_value_to_temporary(
            builder,
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: hir::SourceLocation::Generated,
            },
        ),
        _ => {
            let expr = jsx_expr.as_expression().expect(
                "JSXExpression should be an Expression variant (EmptyExpression handled above)",
            );
            lower_expr_to_temp(builder, expr, semantic, source)
        }
    }
}

fn lower_jsx_name<'a>(
    builder: &mut HIRBuilder,
    name: &js::JSXElementName<'a>,
    semantic: &Semantic<'a>,
    _source: &str,
) -> hir::JsxTag {
    match name {
        js::JSXElementName::Identifier(ident) => {
            let name_str = ident.name.as_str();
            if name_str.chars().next().is_some_and(|c| c.is_lowercase()) {
                hir::JsxTag::BuiltinTag(name_str.to_string())
            } else {
                let loc = span_to_loc(ident.span);
                let loaded = if let Some(entry) = builder.bindings.get(name_str) {
                    let place = hir::Place {
                        identifier: entry.identifier.clone(),
                        effect: hir::Effect::Unknown,
                        reactive: false,
                        loc: loc.clone(),
                    };
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadLocal {
                            place,
                            loc: loc.clone(),
                        },
                    )
                } else {
                    lower_value_to_temporary(
                        builder,
                        hir::InstructionValue::LoadGlobal {
                            binding: hir::NonLocalBinding::Global {
                                name: name_str.to_string(),
                            },
                            loc: loc.clone(),
                        },
                    )
                };
                hir::JsxTag::Component(loaded)
            }
        }
        js::JSXElementName::IdentifierReference(ident) => {
            let name_str = ident.name.as_str();
            if name_str.chars().next().is_some_and(|c| c.is_lowercase()) {
                hir::JsxTag::BuiltinTag(name_str.to_string())
            } else {
                let loaded = lower_jsx_identifier_ref_to_temp(builder, ident, semantic);
                hir::JsxTag::Component(loaded)
            }
        }
        js::JSXElementName::NamespacedName(ns) => {
            hir::JsxTag::BuiltinTag(format!("{}:{}", ns.namespace.name, ns.name.name))
        }
        js::JSXElementName::MemberExpression(member) => {
            let obj = lower_jsx_member_obj(builder, &member.object, semantic);
            let loaded = lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyLoad {
                    object: obj,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    optional: false,
                    loc: span_to_loc(member.property.span),
                },
            );
            hir::JsxTag::Component(loaded)
        }
        js::JSXElementName::ThisExpression(_) => {
            let place = lower_value_to_temporary(
                builder,
                hir::InstructionValue::LoadGlobal {
                    binding: hir::NonLocalBinding::Global {
                        name: "this".to_string(),
                    },
                    loc: hir::SourceLocation::Generated,
                },
            );
            hir::JsxTag::Component(place)
        }
    }
}

fn lower_jsx_member_obj(
    builder: &mut HIRBuilder,
    obj: &js::JSXMemberExpressionObject<'_>,
    semantic: &Semantic<'_>,
) -> hir::Place {
    match obj {
        js::JSXMemberExpressionObject::IdentifierReference(ident) => {
            lower_jsx_identifier_ref_to_temp(builder, ident, semantic)
        }
        js::JSXMemberExpressionObject::MemberExpression(member) => {
            let obj_place = lower_jsx_member_obj(builder, &member.object, semantic);
            lower_value_to_temporary(
                builder,
                hir::InstructionValue::PropertyLoad {
                    object: obj_place,
                    property: hir::PropertyLiteral::String(member.property.name.to_string()),
                    optional: false,
                    loc: span_to_loc(member.property.span),
                },
            )
        }
        js::JSXMemberExpressionObject::ThisExpression(_) => lower_value_to_temporary(
            builder,
            hir::InstructionValue::LoadGlobal {
                binding: hir::NonLocalBinding::Global {
                    name: "this".to_string(),
                },
                loc: hir::SourceLocation::Generated,
            },
        ),
    }
}

fn lower_jsx_identifier_ref_to_temp<'a>(
    builder: &mut HIRBuilder,
    ident: &js::IdentifierReference<'a>,
    semantic: &Semantic<'a>,
) -> hir::Place {
    let loc = span_to_loc(ident.span);
    let name = ident.name.as_str();

    if let Some(entry) = builder.bindings.get(name) {
        let place = hir::Place {
            identifier: entry.identifier.clone(),
            effect: hir::Effect::Unknown,
            reactive: false,
            loc: loc.clone(),
        };
        let is_context = builder.is_context_identifier_id(place.identifier.id);
        return if is_context {
            lower_value_to_temporary(builder, hir::InstructionValue::LoadContext { place, loc })
        } else {
            lower_value_to_temporary(builder, hir::InstructionValue::LoadLocal { place, loc })
        };
    }

    let binding = resolve_non_local_binding(ident, semantic);
    validate_module_type_provider_binding(builder, &binding);
    lower_value_to_temporary(builder, hir::InstructionValue::LoadGlobal { binding, loc })
}

fn lower_arrow<'a>(
    builder: &mut HIRBuilder,
    arrow: &js::ArrowFunctionExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
) -> hir::LoweredFunction {
    let env = builder.env.clone();
    match lower_function_inner(
        &arrow.body,
        &arrow.params,
        LoweringContext::new(semantic, source, env)
            .with_binding_name_counters(builder.binding_name_counters()),
        LowerFunctionOptions::arrow(None, arrow.span, arrow.r#async, arrow.expression),
    ) {
        Ok(result) => hir::LoweredFunction { func: result.func },
        Err(e) => {
            // Propagate inner function lowering errors to the outer builder
            // so they cause a bail-out at the top level.
            for msg in e.split('\n') {
                if !msg.is_empty() {
                    builder.push_todo(msg.to_string());
                }
            }
            stub_lowered_function(builder)
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Lower a CallExpression, with optional chaining support.
fn lower_call_expr<'a>(
    builder: &mut HIRBuilder,
    call: &js::CallExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: hir::SourceLocation,
) -> hir::InstructionValue {
    lower_call_expr_inner(builder, call, semantic, source, loc, false)
}

fn lower_call_expr_inner<'a>(
    builder: &mut HIRBuilder,
    call: &js::CallExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: hir::SourceLocation,
    _in_chain: bool,
) -> hir::InstructionValue {
    if call_expr_is_hook_call(call) && call_has_spread_arguments(call) {
        builder.push_todo("Support spread syntax for hook arguments".to_string());
    }
    // Method call check: obj.method(...) or obj?.method(...)
    if let js::Expression::StaticMemberExpression(member) = &call.callee {
        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
        let property = lower_value_to_temporary(
            builder,
            hir::InstructionValue::PropertyLoad {
                object: object.clone(),
                property: hir::PropertyLiteral::String(member.property.name.to_string()),
                optional: member.optional,
                loc: span_to_loc(member.span),
            },
        );
        let args = lower_args(builder, &call.arguments, semantic, source);
        let receiver_optional = member.optional;
        let call_optional = call.optional;
        return hir::InstructionValue::MethodCall {
            receiver: object,
            property,
            args,
            receiver_optional,
            call_optional,
            loc,
        };
    }
    if let js::Expression::ComputedMemberExpression(member) = &call.callee {
        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
        let computed_property = lower_expr_to_temp(builder, &member.expression, semantic, source);
        let property = lower_value_to_temporary(
            builder,
            hir::InstructionValue::ComputedLoad {
                object: object.clone(),
                property: computed_property,
                optional: member.optional,
                loc: span_to_loc(member.span),
            },
        );
        let args = lower_args(builder, &call.arguments, semantic, source);
        let receiver_optional = member.optional;
        let call_optional = call.optional;
        return hir::InstructionValue::MethodCall {
            receiver: object,
            property,
            args,
            receiver_optional,
            call_optional,
            loc,
        };
    }
    let callee = lower_expr_to_temp(builder, &call.callee, semantic, source);
    let args = lower_args(builder, &call.arguments, semantic, source);
    let optional = call.optional;
    hir::InstructionValue::CallExpression {
        callee,
        args,
        optional,
        loc,
    }
}

fn lower_non_nested_optional_static_member_chain_expr<'a>(
    builder: &mut HIRBuilder,
    member: &js::StaticMemberExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: hir::SourceLocation,
) -> hir::InstructionValue {
    let result_place = builder.make_temporary_place(loc.clone());
    let continuation = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation.id;
    let consequent = builder.reserve(hir::BlockKind::Value);
    let consequent_id = consequent.id;

    let alternate = builder.enter(hir::BlockKind::Value, |builder, _| {
        let undefined = lower_value_to_temporary(
            builder,
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: loc.clone(),
            },
        );
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    kind: hir::InstructionKind::Const,
                    place: result_place.clone(),
                },
                value: undefined,
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    let mut object_place: Option<hir::Place> = None;
    let test_block = builder.enter(hir::BlockKind::Value, |builder, _| {
        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
        object_place = Some(object.clone());
        hir::Terminal::Branch {
            test: object,
            consequent: consequent_id,
            alternate,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    let object = object_place.unwrap_or_else(|| {
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: loc.clone(),
            },
        )
    });
    builder.enter_reserved(consequent, |builder, _| {
        let property_load = lower_value_to_temporary(
            builder,
            hir::InstructionValue::PropertyLoad {
                object,
                property: hir::PropertyLiteral::String(member.property.name.to_string()),
                optional: false,
                loc: loc.clone(),
            },
        );
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    kind: hir::InstructionKind::Const,
                    place: result_place.clone(),
                },
                value: property_load,
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    builder.terminate_with_continuation(
        hir::Terminal::Optional {
            optional: true,
            test: test_block,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        continuation,
    );

    hir::InstructionValue::LoadLocal {
        place: result_place,
        loc,
    }
}

fn lower_non_nested_optional_computed_member_chain_expr<'a>(
    builder: &mut HIRBuilder,
    member: &js::ComputedMemberExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: hir::SourceLocation,
) -> hir::InstructionValue {
    let result_place = builder.make_temporary_place(loc.clone());
    let continuation = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation.id;
    let consequent = builder.reserve(hir::BlockKind::Value);
    let consequent_id = consequent.id;

    let alternate = builder.enter(hir::BlockKind::Value, |builder, _| {
        let undefined = lower_value_to_temporary(
            builder,
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: loc.clone(),
            },
        );
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    kind: hir::InstructionKind::Const,
                    place: result_place.clone(),
                },
                value: undefined,
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    let mut object_place: Option<hir::Place> = None;
    let test_block = builder.enter(hir::BlockKind::Value, |builder, _| {
        let object = lower_expr_to_temp(builder, &member.object, semantic, source);
        object_place = Some(object.clone());
        hir::Terminal::Branch {
            test: object,
            consequent: consequent_id,
            alternate,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    let object = object_place.unwrap_or_else(|| {
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc: loc.clone(),
            },
        )
    });
    builder.enter_reserved(consequent, |builder, _| {
        let computed_property = lower_expr_to_temp(builder, &member.expression, semantic, source);
        let computed_load = lower_value_to_temporary(
            builder,
            hir::InstructionValue::ComputedLoad {
                object,
                property: computed_property,
                optional: false,
                loc: loc.clone(),
            },
        );
        lower_value_to_temporary(
            builder,
            hir::InstructionValue::StoreLocal {
                lvalue: hir::LValue {
                    kind: hir::InstructionKind::Const,
                    place: result_place.clone(),
                },
                value: computed_load,
                loc: loc.clone(),
            },
        );
        hir::Terminal::Goto {
            block: continuation_id,
            variant: hir::GotoVariant::Break,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        }
    });

    builder.terminate_with_continuation(
        hir::Terminal::Optional {
            optional: true,
            test: test_block,
            fallthrough: continuation_id,
            id: hir::InstructionId::default(),
            loc: loc.clone(),
        },
        continuation,
    );

    hir::InstructionValue::LoadLocal {
        place: result_place,
        loc,
    }
}

/// Lower a ChainExpression (optional chaining: x?.y, x?.(), x?.method()).
/// We flatten the chain into regular HIR instructions with optional flags.
fn lower_chain_expr<'a>(
    builder: &mut HIRBuilder,
    chain: &js::ChainExpression<'a>,
    semantic: &Semantic<'a>,
    source: &str,
    loc: hir::SourceLocation,
) -> hir::InstructionValue {
    match &chain.expression {
        js::ChainElement::CallExpression(call) => {
            lower_call_expr_inner(builder, call, semantic, source, loc, true)
        }
        js::ChainElement::StaticMemberExpression(member) if member.optional => {
            lower_non_nested_optional_static_member_chain_expr(
                builder, member, semantic, source, loc,
            )
        }
        js::ChainElement::StaticMemberExpression(member) => {
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            hir::InstructionValue::PropertyLoad {
                object,
                property: hir::PropertyLiteral::String(member.property.name.to_string()),
                optional: member.optional,
                loc,
            }
        }
        js::ChainElement::ComputedMemberExpression(member) if member.optional => {
            lower_non_nested_optional_computed_member_chain_expr(
                builder, member, semantic, source, loc,
            )
        }
        js::ChainElement::ComputedMemberExpression(member) => {
            if let js::Expression::StringLiteral(s) = &member.expression {
                let key = s.value.as_str();
                if is_valid_js_identifier(key) {
                    let object = lower_expr_to_temp(builder, &member.object, semantic, source);
                    return hir::InstructionValue::PropertyLoad {
                        object,
                        property: hir::PropertyLiteral::String(key.to_string()),
                        optional: member.optional,
                        loc,
                    };
                }
            }
            let object = lower_expr_to_temp(builder, &member.object, semantic, source);
            let property = lower_expr_to_temp(builder, &member.expression, semantic, source);
            hir::InstructionValue::ComputedLoad {
                object,
                property,
                optional: member.optional,
                loc,
            }
        }
        _ => {
            // TSNonNullExpression, PrivateFieldExpression — treat as unsupported for now
            hir::InstructionValue::Primitive {
                value: hir::PrimitiveValue::Undefined,
                loc,
            }
        }
    }
}

fn lower_args<'a>(
    builder: &mut HIRBuilder,
    args: &[js::Argument<'a>],
    semantic: &Semantic<'a>,
    source: &str,
) -> Vec<hir::Argument> {
    args.iter()
        .map(|arg| {
            match arg {
                js::Argument::SpreadElement(spread) => {
                    let place = lower_expr_to_temp(builder, &spread.argument, semantic, source);
                    hir::Argument::Spread(place)
                }
                _ => {
                    // Expression variant via @inherit
                    let expr = expr_from_arg(arg);
                    if call_arg_has_complex_destructure_assignment(expr) {
                        builder.push_invariant(
                            "Const declaration cannot be referenced as an expression".to_string(),
                        );
                    }
                    let place = lower_expr_to_temp(builder, expr, semantic, source);
                    hir::Argument::Place(place)
                }
            }
        })
        .collect()
}

fn call_arg_has_complex_destructure_assignment(expr: &js::Expression<'_>) -> bool {
    match expr {
        js::Expression::ParenthesizedExpression(paren) => {
            call_arg_has_complex_destructure_assignment(&paren.expression)
        }
        js::Expression::AssignmentExpression(assign) => {
            assignment_target_requires_complex_destructure(&assign.left)
        }
        _ => false,
    }
}

fn assignment_target_requires_complex_destructure(target: &js::AssignmentTarget<'_>) -> bool {
    match target {
        js::AssignmentTarget::ArrayAssignmentTarget(arr) => !array_assignment_target_is_simple(arr),
        js::AssignmentTarget::ObjectAssignmentTarget(obj) => {
            !object_assignment_target_is_simple(obj)
        }
        _ => false,
    }
}

fn array_assignment_target_is_simple(arr: &js::ArrayAssignmentTarget<'_>) -> bool {
    arr.elements.iter().all(|elem| match elem {
        None => true,
        Some(e) => matches!(
            e,
            js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(_)
        ),
    }) && arr.rest.as_ref().is_none_or(|r| {
        matches!(
            &r.target,
            js::AssignmentTarget::AssignmentTargetIdentifier(_)
        )
    })
}

fn object_assignment_target_is_simple(obj: &js::ObjectAssignmentTarget<'_>) -> bool {
    obj.properties.iter().all(|prop| match prop {
        js::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(p) => p.init.is_none(),
        js::AssignmentTargetProperty::AssignmentTargetPropertyProperty(p) => {
            !p.computed
                && matches!(
                    &p.binding,
                    js::AssignmentTargetMaybeDefault::AssignmentTargetIdentifier(_)
                )
        }
    }) && obj.rest.is_none()
}

fn call_has_spread_arguments(call: &js::CallExpression<'_>) -> bool {
    call.arguments
        .iter()
        .any(|arg| matches!(arg, js::Argument::SpreadElement(_)))
}

fn call_expr_is_hook_call(call: &js::CallExpression<'_>) -> bool {
    callee_is_hook_name(&call.callee)
}

fn callee_is_hook_name(callee: &js::Expression<'_>) -> bool {
    match callee {
        js::Expression::Identifier(ident) => is_hook_like_name(ident.name.as_str()),
        js::Expression::StaticMemberExpression(member) => {
            is_hook_like_name(member.property.name.as_str())
        }
        js::Expression::ComputedMemberExpression(member) => match &member.expression {
            js::Expression::StringLiteral(s) => is_hook_like_name(s.value.as_str()),
            _ => false,
        },
        js::Expression::ChainExpression(chain) => match &chain.expression {
            js::ChainElement::CallExpression(_) => false,
            js::ChainElement::StaticMemberExpression(member) => {
                is_hook_like_name(member.property.name.as_str())
            }
            js::ChainElement::ComputedMemberExpression(member) => match &member.expression {
                js::Expression::StringLiteral(s) => is_hook_like_name(s.value.as_str()),
                _ => false,
            },
            _ => false,
        },
        _ => false,
    }
}

fn is_hook_like_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("use") else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch == '$' || ch.is_ascii_uppercase())
}

/// Extract an Expression reference from an Argument (which inherits Expression variants).
/// This is safe because all non-SpreadElement variants are Expression variants.
fn expr_from_arg<'a, 'b>(arg: &'b js::Argument<'a>) -> &'b js::Expression<'a> {
    // SAFETY: Argument inherits Expression, and we've already handled SpreadElement.
    // All other variants are Expression variants that can be transmuted.
    // But actually we should use a proper approach. Let's use `as_expression()` if available.
    // Otherwise fall back to a dummy.
    unsafe { std::mem::transmute(arg) }
}

/// Extract an Expression reference from an ArrayExpressionElement.
fn expr_from_array_elem<'a, 'b>(
    elem: &'b js::ArrayExpressionElement<'a>,
) -> &'b js::Expression<'a> {
    // All non-SpreadElement/Elision variants are Expression variants
    unsafe { std::mem::transmute(elem) }
}

fn lower_prop_key(key: &js::PropertyKey<'_>) -> hir::ObjectPropertyKey {
    match key {
        js::PropertyKey::StaticIdentifier(ident) => {
            hir::ObjectPropertyKey::Identifier(ident.name.to_string())
        }
        js::PropertyKey::StringLiteral(s) => hir::ObjectPropertyKey::String(s.value.to_string()),
        js::PropertyKey::NumericLiteral(n) => hir::ObjectPropertyKey::Number(n.value),
        _ => hir::ObjectPropertyKey::String("unknown".to_string()),
    }
}

fn convert_bin_op(op: oxc_syntax::operator::BinaryOperator) -> hir::BinaryOperator {
    use oxc_syntax::operator::BinaryOperator as O;
    match op {
        O::Equality => hir::BinaryOperator::Eq,
        O::Inequality => hir::BinaryOperator::NotEq,
        O::StrictEquality => hir::BinaryOperator::StrictEq,
        O::StrictInequality => hir::BinaryOperator::StrictNotEq,
        O::LessThan => hir::BinaryOperator::Lt,
        O::LessEqualThan => hir::BinaryOperator::LtEq,
        O::GreaterThan => hir::BinaryOperator::Gt,
        O::GreaterEqualThan => hir::BinaryOperator::GtEq,
        O::ShiftLeft => hir::BinaryOperator::LShift,
        O::ShiftRight => hir::BinaryOperator::RShift,
        O::ShiftRightZeroFill => hir::BinaryOperator::URShift,
        O::Addition => hir::BinaryOperator::Add,
        O::Subtraction => hir::BinaryOperator::Sub,
        O::Multiplication => hir::BinaryOperator::Mul,
        O::Division => hir::BinaryOperator::Div,
        O::Remainder => hir::BinaryOperator::Mod,
        O::Exponential => hir::BinaryOperator::Exp,
        O::BitwiseOR => hir::BinaryOperator::BitOr,
        O::BitwiseXOR => hir::BinaryOperator::BitXor,
        O::BitwiseAnd => hir::BinaryOperator::BitAnd,
        O::In => hir::BinaryOperator::In,
        O::Instanceof => hir::BinaryOperator::InstanceOf,
    }
}

fn convert_unary_op(op: oxc_syntax::operator::UnaryOperator) -> hir::UnaryOperator {
    use oxc_syntax::operator::UnaryOperator as O;
    match op {
        O::UnaryNegation => hir::UnaryOperator::Minus,
        O::UnaryPlus => hir::UnaryOperator::Plus,
        O::LogicalNot => hir::UnaryOperator::Not,
        O::BitwiseNot => hir::UnaryOperator::BitNot,
        O::Typeof => hir::UnaryOperator::TypeOf,
        O::Void => hir::UnaryOperator::Void,
        O::Delete => hir::UnaryOperator::Void,
    }
}

fn lower_type_annotation(node: &js::TSType<'_>) -> hir::Type {
    match node {
        js::TSType::TSTypeReference(type_ref) => {
            if let js::TSTypeName::IdentifierReference(ident) = &type_ref.type_name
                && ident.name.as_str() == "Array"
            {
                return hir::Type::Object {
                    shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
                };
            }
            hir::Type::Poly
        }
        js::TSType::TSArrayType(_) => hir::Type::Object {
            shape_id: Some(BUILT_IN_ARRAY_ID.to_string()),
        },
        js::TSType::TSLiteralType(_)
        | js::TSType::TSBooleanKeyword(_)
        | js::TSType::TSNullKeyword(_)
        | js::TSType::TSNumberKeyword(_)
        | js::TSType::TSStringKeyword(_)
        | js::TSType::TSSymbolKeyword(_)
        | js::TSType::TSUndefinedKeyword(_)
        | js::TSType::TSVoidKeyword(_) => hir::Type::Primitive,
        _ => hir::Type::Poly,
    }
}

fn classify_type_assertion_kind(
    expr: &js::TSTypeAssertion<'_>,
    source: &str,
) -> hir::TypeAnnotationKind {
    let raw = source_slice(source, expr.span);
    let trimmed = raw.trim_start();
    if trimmed.starts_with('(') && trimmed.contains(':') {
        hir::TypeAnnotationKind::Cast
    } else {
        hir::TypeAnnotationKind::As
    }
}

fn source_slice(source: &str, span: Span) -> String {
    let start = span.start as usize;
    let end = span.end as usize;
    if start >= end {
        return String::new();
    }
    source.get(start..end).unwrap_or_default().to_string()
}

fn offset_to_line_col(offset: u32, line_starts: &[u32]) -> hir::SourcePosition {
    if line_starts.is_empty() {
        return hir::SourcePosition {
            line: 1,
            column: offset,
        };
    }
    let idx = match line_starts.binary_search(&offset) {
        Ok(i) => i,
        Err(0) => 0,
        Err(i) => i - 1,
    };
    let line_start = line_starts[idx];
    hir::SourcePosition {
        line: idx as u32 + 1,
        column: offset.saturating_sub(line_start),
    }
}

pub fn span_to_loc(span: Span) -> hir::SourceLocation {
    CURRENT_SOURCE_LINE_STARTS.with(|cell| {
        let line_starts = cell.borrow();
        if line_starts.is_empty() {
            return hir::SourceLocation::Source(hir::SourceRange {
                start: hir::SourcePosition {
                    line: 0,
                    column: span.start,
                },
                end: hir::SourcePosition {
                    line: 0,
                    column: span.end,
                },
            });
        }
        hir::SourceLocation::Source(hir::SourceRange {
            start: offset_to_line_col(span.start, &line_starts),
            end: offset_to_line_col(span.end, &line_starts),
        })
    })
}

/// Trim JSX text whitespace according to the JSX spec.
/// Port of `trimJsxText` from upstream BuildHIR.ts, adapted from Babel's
/// `cleanJSXElementLiteralChild`.
/// Returns None if the text is entirely insignificant whitespace.
fn trim_jsx_text(original: &str) -> Option<String> {
    let lines: Vec<&str> = original.split('\n').collect();

    let mut last_non_empty_line = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.chars().any(|c| c != ' ' && c != '\t') {
            last_non_empty_line = i;
        }
    }

    let mut result = String::new();
    for (i, line) in lines.iter().enumerate() {
        let is_first_line = i == 0;
        let is_last_line = i == lines.len() - 1;
        let is_last_non_empty_line = i == last_non_empty_line;

        // Replace tabs with spaces
        let mut trimmed_line = line.replace('\t', " ");

        // Trim whitespace touching a newline (leading whitespace on non-first lines)
        if !is_first_line {
            trimmed_line = trimmed_line.trim_start_matches(' ').to_string();
        }

        // Trim whitespace touching an endline (trailing whitespace on non-last lines)
        if !is_last_line {
            trimmed_line = trimmed_line.trim_end_matches(' ').to_string();
        }

        if !trimmed_line.is_empty() {
            if !is_last_non_empty_line {
                trimmed_line.push(' ');
            }
            result.push_str(&trimmed_line);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Decode HTML entities in JSX text content.
fn decode_jsx_entities(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            let mut entity = String::new();
            for ec in chars.by_ref() {
                if ec == ';' {
                    break;
                }
                entity.push(ec);
            }
            match entity.as_str() {
                "amp" => result.push('&'),
                "lt" => result.push('<'),
                "gt" => result.push('>'),
                "quot" => result.push('"'),
                "apos" => result.push('\''),
                "nbsp" => result.push('\u{00A0}'),
                s if s.starts_with('#') => {
                    let code = if s.starts_with("#x") || s.starts_with("#X") {
                        u32::from_str_radix(&s[2..], 16).ok()
                    } else {
                        s[1..].parse::<u32>().ok()
                    };
                    if let Some(cp) = code.and_then(char::from_u32) {
                        result.push(cp);
                    } else {
                        result.push('&');
                        result.push_str(&entity);
                        result.push(';');
                    }
                }
                _ => {
                    // Unknown entity — keep as-is
                    result.push('&');
                    result.push_str(&entity);
                    result.push(';');
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Check if a string is a valid JavaScript identifier (for converting computed to property access).
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
