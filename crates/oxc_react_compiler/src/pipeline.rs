//! Compiler pipeline — orchestrates the compilation passes.
//!
//! Port of `Pipeline.ts` from upstream.
//!
//! The pipeline runs in 7 phases:
//! 1. HIR Construction (passes 1-3)
//! 2. HIR Pre-processing (passes 4-9)
//! 3. SSA + Analysis (passes 10-18)
//! 4. Mutation/Aliasing (passes 19-25)
//! 5. Validation (passes 26-34)
//! 6. Reactive Scope Construction (passes 35-54)
//! 7. Reactive Function + Codegen (passes 55-72)

use std::cell::{Cell, RefCell};

use hmac::{Hmac, Mac};
use oxc_ast::ast;
use oxc_span::GetSpan;
use sha2::Sha256;

use crate::CompileResult;
use crate::codegen_backend::{
    CompiledArrayPattern, CompiledBindingPattern, CompiledFunction, CompiledInitializer,
    CompiledObjectPattern, CompiledObjectPatternProperty, CompiledOutlinedFunction, CompiledParam,
    CompiledParamPrefixStatement, CompiledPropertyKey, ModuleEmitArgs,
    SynthesizedDefaultParamCache,
};
use crate::error::CompilerError;
use crate::hir::build;
use crate::hir::types::HIRFunction;
use crate::optimization;
use crate::optimization::constant_propagation;
use crate::optimization::dead_code_elimination;
use crate::optimization::drop_manual_memoization;
use crate::optimization::inline_iifes;
use crate::options::{CompilationMode, PanicThreshold, PluginOptions};
use crate::reactive_scopes::align_scopes;
use crate::reactive_scopes::infer_reactive;
use crate::reactive_scopes::infer_scope_variables;
use crate::reactive_scopes::merge_overlapping_scopes;
use crate::ssa::eliminate_redundant_phi;
use crate::ssa::enter_ssa;
use crate::ssa::rewrite_instruction_kinds;
use crate::type_inference;

// Phase 2: HIR pre-processing passes
use crate::hir::merge_consecutive_blocks;
use crate::hir::prune_maybe_throws;

// Phase 3: Validation passes
use crate::inference::infer_effect_dependencies;
use crate::validation::validate_context_variable_lvalues;
use crate::validation::validate_hooks_usage;
use crate::validation::validate_locals_not_reassigned_after_render;
use crate::validation::validate_no_capitalized_calls;
use crate::validation::validate_no_derived_computations_in_effects;
use crate::validation::validate_no_freezing_known_mutable_functions;
use crate::validation::validate_no_impure_functions_in_render;
use crate::validation::validate_no_jsx_in_try_statement;
use crate::validation::validate_no_ref_access_in_render;
use crate::validation::validate_no_set_state_in_effects;
use crate::validation::validate_no_set_state_in_render;
use crate::validation::validate_static_components;
use crate::validation::validate_use_memo;

// Phase 4: Aliasing analysis
use crate::inference::analyse_functions;
use crate::inference::infer_mutation_aliasing_effects;

// Phase 5: Scope construction (HIR-level)
use crate::hir::build_reactive_scope_terminals;
use crate::hir::flatten_reactive_loops;
use crate::hir::flatten_scopes_with_hooks;
use crate::hir::propagate_scope_dependencies_hir;
use crate::hir::prune_unused_labels;
use crate::reactive_scopes::align_method_call_scopes;
use crate::reactive_scopes::align_object_method_scopes;
use crate::reactive_scopes::memoize_fbt_operands;

// Phase 6: BuildReactiveFunction + post-reactive passes + codegen
use crate::reactive_scopes::build_reactive_function;
use crate::reactive_scopes::extract_scope_destructuring;
use crate::reactive_scopes::merge_scopes_invalidate_together;
use crate::reactive_scopes::promote_used_temporaries;
use crate::reactive_scopes::propagate_early_returns;
use crate::reactive_scopes::prune_always_invalidating_reactive;
use crate::reactive_scopes::prune_hoisted_contexts;
use crate::reactive_scopes::prune_initialization_dependencies;
use crate::reactive_scopes::prune_non_escaping_scopes;
use crate::reactive_scopes::prune_non_reactive_deps_reactive;
use crate::reactive_scopes::prune_unused_labels_reactive;
use crate::reactive_scopes::prune_unused_lvalues;
use crate::reactive_scopes::prune_unused_scopes_reactive;
use crate::reactive_scopes::rename_variables;
use crate::reactive_scopes::stabilize_block_ids;

// ---------------------------------------------------------------------------
// Upstream-compatible skip logic (from Entrypoint/Program.ts)
// ---------------------------------------------------------------------------

const OPT_OUT_DIRECTIVES: &[&str] = &["use no forget", "use no memo"];

thread_local! {
    static RETRY_NO_MEMO_MODE: Cell<bool> = const { Cell::new(false) };
    static FILE_HAD_PIPELINE_ERROR: Cell<bool> = const { Cell::new(false) };
    static CURRENT_FILENAME: RefCell<String> = const { RefCell::new(String::new()) };
    static FLOW_COMPONENT_NAMES: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    static FLOW_HOOK_NAMES: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());
    static FAST_REFRESH_SOURCE_HASH: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn set_fast_refresh_source_hash(hash: Option<String>) {
    FAST_REFRESH_SOURCE_HASH.with(|slot| {
        *slot.borrow_mut() = hash;
    });
}

fn get_fast_refresh_source_hash() -> Option<String> {
    FAST_REFRESH_SOURCE_HASH.with(|slot| slot.borrow().clone())
}

const FLOW_CAST_REWRITE_MARKER: &str = "/*__FLOW_CAST__*/";

fn compute_fast_refresh_source_hash(source: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mac =
        HmacSha256::new_from_slice(source.as_bytes()).expect("HMAC accepts arbitrary key sizes");
    let bytes = mac.finalize().into_bytes();
    let mut hash = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut hash, "{:02x}", b);
    }
    hash
}

struct FastRefreshHashGuard;

impl Drop for FastRefreshHashGuard {
    fn drop(&mut self) {
        set_fast_refresh_source_hash(None);
    }
}

/// Pre-process Flow-specific syntax that OXC can't parse:
/// - `component Name(params) { body }` → `function Name(params) { body }`
/// - `hook useName(params) { body }` → `function useName(params) { body }`
/// - `export default component Name(...)` → `export default function Name(...)`
fn preprocess_flow_syntax(source: &str) -> String {
    let mut result = String::with_capacity(source.len());
    let mut saw_non_comment_code = false;
    for line in source.lines() {
        let trimmed = line.trim();
        // Strip only file-leading @flow pragmas. Mid-file comments should be preserved.
        if !saw_non_comment_code
            && (trimmed == "//@flow"
                || trimmed == "// @flow"
                || trimmed.starts_with("//@flow ")
                || trimmed.starts_with("// @flow "))
        {
            continue;
        }
        if let Some(transformed_component) = transform_simple_flow_component_line(line) {
            result.push_str(&transformed_component);
            result.push('\n');
            saw_non_comment_code = true;
            continue;
        }
        // Replace `component Name(` with `function Name(`
        // Handle: `component Name(`, `export default component Name(`
        let mut processed = line.to_string();
        if let Some(idx) = find_flow_keyword(&processed, "component") {
            // Check that what follows is an uppercase name + (
            let after = processed[idx + "component".len()..].trim_start();
            if after.starts_with(|c: char| c.is_uppercase()) {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "component".len()..]
                );
            }
        }
        // Replace `hook useName(` with `function useName(`
        if let Some(idx) = find_flow_keyword(&processed, "hook") {
            let after = processed[idx + "hook".len()..].trim_start();
            if after.starts_with("use") {
                processed = format!(
                    "{}function{}",
                    &processed[..idx],
                    &processed[idx + "hook".len()..]
                );
            }
        }
        if !trimmed.is_empty()
            && !trimmed.starts_with("//")
            && !trimmed.starts_with("/*")
            && !trimmed.starts_with('*')
            && !trimmed.starts_with("*/")
        {
            saw_non_comment_code = true;
        }
        result.push_str(&processed);
        result.push('\n');
    }
    // Remove trailing newline added by the loop if source didn't end with one
    if !source.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Best-effort stripper for Flow-style function signature annotations.
///
/// This is only used as a parse fallback when both JS and TS parsing fail.
/// It removes:
/// - Param annotations in `function foo(a: T, b?: U) {}`
/// - Return annotations in `function foo(a): R {}`
fn strip_flow_function_signature_types(source: &str) -> String {
    fn is_ident_continue(ch: char) -> bool {
        ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
    }

    fn strip_optional_suffix(param: &str) -> String {
        let trimmed = param.trim_end();
        if let Some(stripped) = trimmed.strip_suffix('?') {
            stripped.to_string()
        } else {
            trimmed.to_string()
        }
    }

    fn strip_single_param(param: &str) -> String {
        let leading_ws_len = param.len() - param.trim_start().len();
        let trailing_ws_len = param.len() - param.trim_end().len();
        let leading = &param[..leading_ws_len];
        let trailing = &param[param.len() - trailing_ws_len..];
        let core = param.trim();
        if core.is_empty() {
            return param.to_string();
        }

        let mut depth_paren = 0usize;
        let mut depth_brace = 0usize;
        let mut depth_bracket = 0usize;
        let mut depth_angle = 0usize;
        let mut colon_at: Option<usize> = None;
        let mut assign_at: Option<usize> = None;
        for (idx, ch) in core.char_indices() {
            match ch {
                '(' => depth_paren += 1,
                ')' => depth_paren = depth_paren.saturating_sub(1),
                '{' => depth_brace += 1,
                '}' => depth_brace = depth_brace.saturating_sub(1),
                '[' => depth_bracket += 1,
                ']' => depth_bracket = depth_bracket.saturating_sub(1),
                '<' => depth_angle += 1,
                '>' => depth_angle = depth_angle.saturating_sub(1),
                ':' if depth_paren == 0
                    && depth_brace == 0
                    && depth_bracket == 0
                    && depth_angle == 0
                    && colon_at.is_none() =>
                {
                    colon_at = Some(idx);
                }
                '=' if depth_paren == 0
                    && depth_brace == 0
                    && depth_bracket == 0
                    && depth_angle == 0
                    && assign_at.is_none()
                    && core[idx + ch.len_utf8()..]
                        .chars()
                        .next()
                        .is_none_or(|next| next != '>') =>
                {
                    assign_at = Some(idx);
                }
                _ => {}
            }
        }

        let stripped_core = if let Some(colon) = colon_at {
            let left = strip_optional_suffix(core[..colon].trim_end());
            if let Some(eq) = assign_at {
                if eq > colon {
                    format!("{} {}", left.trim_end(), core[eq..].trim_start())
                } else {
                    left
                }
            } else {
                left
            }
        } else {
            strip_optional_suffix(core)
        };

        format!("{}{}{}", leading, stripped_core, trailing)
    }

    fn strip_param_list(params: &str) -> String {
        let mut out = String::with_capacity(params.len());
        let mut cur = String::new();
        let mut depth_paren = 0usize;
        let mut depth_brace = 0usize;
        let mut depth_bracket = 0usize;
        let mut depth_angle = 0usize;

        for ch in params.chars() {
            match ch {
                ',' if depth_paren == 0
                    && depth_brace == 0
                    && depth_bracket == 0
                    && depth_angle == 0 =>
                {
                    out.push_str(&strip_single_param(&cur));
                    out.push(',');
                    cur.clear();
                }
                '(' => {
                    depth_paren += 1;
                    cur.push(ch);
                }
                ')' => {
                    depth_paren = depth_paren.saturating_sub(1);
                    cur.push(ch);
                }
                '{' => {
                    depth_brace += 1;
                    cur.push(ch);
                }
                '}' => {
                    depth_brace = depth_brace.saturating_sub(1);
                    cur.push(ch);
                }
                '[' => {
                    depth_bracket += 1;
                    cur.push(ch);
                }
                ']' => {
                    depth_bracket = depth_bracket.saturating_sub(1);
                    cur.push(ch);
                }
                '<' => {
                    depth_angle += 1;
                    cur.push(ch);
                }
                '>' => {
                    depth_angle = depth_angle.saturating_sub(1);
                    cur.push(ch);
                }
                _ => cur.push(ch),
            }
        }
        if !cur.is_empty() {
            out.push_str(&strip_single_param(&cur));
        }
        out
    }

    fn strip_return_annotation(rest: &str) -> String {
        let trimmed_start = rest.trim_start();
        if !trimmed_start.starts_with(':') {
            return rest.to_string();
        }
        let mut out = String::new();
        let ws_prefix_len = rest.len() - trimmed_start.len();
        out.push_str(&rest[..ws_prefix_len]);

        let chars: Vec<char> = trimmed_start.chars().collect();
        let mut i = 1usize; // skip initial ':'
        let mut depth_paren = 0usize;
        let mut depth_brace = 0usize;
        let mut depth_bracket = 0usize;
        let mut depth_angle = 0usize;
        while i < chars.len() {
            let ch = chars[i];
            let at_top =
                depth_paren == 0 && depth_brace == 0 && depth_bracket == 0 && depth_angle == 0;
            if at_top {
                if ch == '{' {
                    break;
                }
                if ch == '=' && i + 1 < chars.len() && chars[i + 1] == '>' {
                    break;
                }
            }
            match ch {
                '(' => depth_paren += 1,
                ')' => depth_paren = depth_paren.saturating_sub(1),
                '{' => depth_brace += 1,
                '}' => depth_brace = depth_brace.saturating_sub(1),
                '[' => depth_bracket += 1,
                ']' => depth_bracket = depth_bracket.saturating_sub(1),
                '<' => depth_angle += 1,
                '>' => depth_angle = depth_angle.saturating_sub(1),
                _ => {}
            }
            i += 1;
        }

        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        for ch in &chars[i..] {
            out.push(*ch);
        }
        out
    }

    let mut output = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let remaining = &source[i..];
        if remaining.starts_with("function")
            && (i == 0 || !is_ident_continue(source[..i].chars().last().unwrap_or(' ')))
        {
            output.push_str("function");
            i += "function".len();

            // Copy until the opening paren of the signature.
            while i < bytes.len() {
                let ch = source[i..].chars().next().unwrap();
                output.push(ch);
                i += ch.len_utf8();
                if ch == '(' {
                    break;
                }
            }

            // Capture params up to matching ')' and strip their type annotations.
            let mut params = String::new();
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                let ch = source[i..].chars().next().unwrap();
                i += ch.len_utf8();
                match ch {
                    '(' => {
                        depth += 1;
                        params.push(ch);
                    }
                    ')' => {
                        depth -= 1;
                        if depth > 0 {
                            params.push(ch);
                        }
                    }
                    _ => params.push(ch),
                }
            }
            output.push_str(&strip_param_list(&params));
            output.push(')');

            // Copy/strip return type annotation if present.
            let mut tail = String::new();
            while i < bytes.len() {
                let ch = source[i..].chars().next().unwrap();
                if ch == '{' || (ch == '=' && source[i + ch.len_utf8()..].starts_with('>')) {
                    break;
                }
                tail.push(ch);
                i += ch.len_utf8();
            }
            output.push_str(&strip_return_annotation(&tail));
            continue;
        }

        let ch = source[i..].chars().next().unwrap();
        output.push(ch);
        i += ch.len_utf8();
    }
    output
}

/// Rewrite Flow cast expressions into TS `as` assertions for parse fallback.
///
/// Example:
/// `(value: Foo)` -> `(value as /*__FLOW_CAST__*/ Foo)`
///
/// The marker comment allows HIR lowering to recover cast-style emission.
pub(crate) fn rewrite_flow_cast_expressions(source: &str) -> String {
    fn split_flow_cast_inner(inner: &str) -> Option<(String, String)> {
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
        let left = inner[..colon].trim_end();
        let right = inner[colon + 1..].trim_start();
        if left.is_empty() || right.is_empty() {
            return None;
        }
        Some((left.to_string(), right.to_string()))
    }

    let mut out = String::with_capacity(source.len());
    let mut paren_stack: Vec<usize> = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        if source[i..].starts_with("//") {
            while i < bytes.len() {
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                if ch == '\n' {
                    break;
                }
            }
            continue;
        }
        if source[i..].starts_with("/*") {
            out.push('/');
            out.push('*');
            i += 2;
            while i < bytes.len() {
                if source[i..].starts_with("*/") {
                    out.push('*');
                    out.push('/');
                    i += 2;
                    break;
                }
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            continue;
        }

        let ch = source[i..].chars().next().unwrap();
        if ch == '\'' || ch == '"' || ch == '`' {
            let quote = ch;
            out.push(ch);
            i += ch.len_utf8();
            let mut escaped = false;
            while i < bytes.len() {
                let c = source[i..].chars().next().unwrap();
                out.push(c);
                i += c.len_utf8();
                if escaped {
                    escaped = false;
                    continue;
                }
                if c == '\\' {
                    escaped = true;
                    continue;
                }
                if c == quote {
                    break;
                }
            }
            continue;
        }

        if ch == '(' {
            paren_stack.push(out.len());
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        if ch == ')' {
            out.push(ch);
            i += ch.len_utf8();

            let mut j = i;
            while j < bytes.len() {
                let next = source[j..].chars().next().unwrap();
                if !next.is_whitespace() {
                    break;
                }
                j += next.len_utf8();
            }
            let followed_by_arrow = source[j..].starts_with("=>");

            if let Some(open_idx) = paren_stack.pop()
                && !followed_by_arrow
            {
                let close_idx = out.len() - 1;
                if open_idx < close_idx {
                    let inner = &out[open_idx + 1..close_idx];
                    if let Some((left, right)) = split_flow_cast_inner(inner) {
                        let mut rewritten = String::new();
                        rewritten.push('(');
                        rewritten.push_str(&left);
                        rewritten.push_str(" as ");
                        rewritten.push_str(FLOW_CAST_REWRITE_MARKER);
                        rewritten.push(' ');
                        rewritten.push_str(&right);
                        rewritten.push(')');
                        out.replace_range(open_idx..=close_idx, &rewritten);
                    }
                }
            }
            continue;
        }

        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

/// Rewrite Flow `component`-origin functions with multiple positional params
/// into a single object-destructuring props param.
///
/// Example:
/// `function Component(a, b) {` -> `function Component({ a, b }) {`
fn rewrite_flow_component_param_lists(source: &str) -> String {
    fn is_ident_start(ch: char) -> bool {
        ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
    }

    fn is_ident_continue(ch: char) -> bool {
        ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
    }

    fn split_top_level_params(params: &str) -> Option<Vec<String>> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut depth_paren = 0usize;
        let mut depth_brace = 0usize;
        let mut depth_bracket = 0usize;
        let mut depth_angle = 0usize;
        for ch in params.chars() {
            match ch {
                ',' if depth_paren == 0
                    && depth_brace == 0
                    && depth_bracket == 0
                    && depth_angle == 0 =>
                {
                    out.push(cur.trim().to_string());
                    cur.clear();
                }
                '(' => {
                    depth_paren += 1;
                    cur.push(ch);
                }
                ')' => {
                    depth_paren = depth_paren.saturating_sub(1);
                    cur.push(ch);
                }
                '{' => {
                    depth_brace += 1;
                    cur.push(ch);
                }
                '}' => {
                    depth_brace = depth_brace.saturating_sub(1);
                    cur.push(ch);
                }
                '[' => {
                    depth_bracket += 1;
                    cur.push(ch);
                }
                ']' => {
                    depth_bracket = depth_bracket.saturating_sub(1);
                    cur.push(ch);
                }
                '<' => {
                    depth_angle += 1;
                    cur.push(ch);
                }
                '>' => {
                    depth_angle = depth_angle.saturating_sub(1);
                    cur.push(ch);
                }
                _ => cur.push(ch),
            }
        }
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
        Some(out)
    }

    fn parse_identifier(param: &str) -> Option<String> {
        let trimmed = param.trim();
        if trimmed.is_empty() || trimmed.starts_with("...") {
            return None;
        }
        let mut chars = trimmed.chars();
        let first = chars.next()?;
        if !is_ident_start(first) {
            return None;
        }
        let mut name = String::new();
        name.push(first);
        let mut rest_start = first.len_utf8();
        for ch in chars {
            if is_ident_continue(ch) {
                name.push(ch);
                rest_start += ch.len_utf8();
                continue;
            }
            break;
        }

        let mut rest = trimmed[rest_start..].trim_start();
        if let Some(stripped) = rest.strip_prefix('?') {
            rest = stripped.trim_start();
        }

        if !rest.is_empty() && !rest.starts_with(':') {
            return None;
        }
        Some(name)
    }

    let flow_names = FLOW_COMPONENT_NAMES.with(|set| set.borrow().clone());
    if flow_names.is_empty() {
        return source.to_string();
    }

    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let remaining = &source[i..];
        if remaining.starts_with("function")
            && (i == 0 || !source[..i].chars().last().is_some_and(is_ident_continue))
        {
            out.push_str("function");
            i += "function".len();

            while i < bytes.len()
                && source[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }

            let name_start = i;
            while i < bytes.len() && source[i..].chars().next().is_some_and(is_ident_continue) {
                i += source[i..].chars().next().unwrap().len_utf8();
            }
            let name = &source[name_start..i];
            out.push_str(name);

            if !flow_names.contains(name) {
                continue;
            }

            while i < bytes.len()
                && source[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_whitespace())
            {
                let ch = source[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
            if i >= bytes.len() || !source[i..].starts_with('(') {
                continue;
            }
            i += 1; // consume '('

            let mut depth = 1usize;
            let mut params = String::new();
            while i < bytes.len() && depth > 0 {
                let ch = source[i..].chars().next().unwrap();
                i += ch.len_utf8();
                match ch {
                    '(' => {
                        depth += 1;
                        params.push(ch);
                    }
                    ')' => {
                        depth -= 1;
                        if depth > 0 {
                            params.push(ch);
                        }
                    }
                    _ => params.push(ch),
                }
            }

            let Some(parts) = split_top_level_params(&params) else {
                out.push('(');
                out.push_str(&params);
                out.push(')');
                continue;
            };
            let mut names = Vec::new();
            let mut ok = true;
            for part in &parts {
                let Some(name) = parse_identifier(part) else {
                    ok = false;
                    break;
                };
                names.push(name);
            }
            if ok && names.len() > 1 {
                out.push('(');
                out.push('{');
                out.push(' ');
                out.push_str(&names.join(", "));
                out.push(' ');
                out.push('}');
                out.push(')');
            } else {
                out.push('(');
                out.push_str(&params);
                out.push(')');
            }
            continue;
        }

        let ch = source[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn collect_flow_component_names(source: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for line in source.lines() {
        let Some(idx) = find_flow_keyword(line, "component") else {
            continue;
        };
        let after = line[idx + "component".len()..].trim_start();
        if !after.starts_with(|c: char| c.is_uppercase()) {
            continue;
        }
        let Some(name_end) = after
            .char_indices()
            .take_while(|(_, c)| is_identifier_char(*c))
            .map(|(i, c)| i + c.len_utf8())
            .last()
        else {
            continue;
        };
        let name = &after[..name_end];
        if is_valid_js_identifier(name) {
            names.insert(name.to_string());
        }
    }
    names
}

fn is_flow_component_name(name: &str) -> bool {
    FLOW_COMPONENT_NAMES.with(|set| set.borrow().contains(name))
}

fn is_flow_hook_name(name: &str) -> bool {
    FLOW_HOOK_NAMES.with(|set| set.borrow().contains(name))
}

/// Collect names of Flow `hook` declarations from source.
fn collect_flow_hook_names(source: &str) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for line in source.lines() {
        let Some(idx) = find_flow_keyword(line, "hook") else {
            continue;
        };
        let after = line[idx + "hook".len()..].trim_start();
        if !after.starts_with("use") {
            continue;
        }
        let Some(name_end) = after
            .char_indices()
            .take_while(|(_, c)| is_identifier_char(*c))
            .map(|(i, c)| i + c.len_utf8())
            .last()
        else {
            continue;
        };
        let name = &after[..name_end];
        if is_valid_js_identifier(name) {
            names.insert(name.to_string());
        }
    }
    names
}

fn transform_simple_flow_component_line(line: &str) -> Option<String> {
    let idx = find_flow_keyword(line, "component")?;
    let prefix = &line[..idx];
    let is_export_prefixed = prefix.trim_start().starts_with("export");

    let after_keyword = line[idx + "component".len()..].trim_start();
    if !after_keyword.starts_with(|c: char| c.is_uppercase()) {
        return None;
    }

    let name_end = after_keyword
        .char_indices()
        .take_while(|(_, c)| is_identifier_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .last()?;
    let name = &after_keyword[..name_end];

    let rest = after_keyword[name_end..].trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let close = rest.find(')')?;
    let params = rest[1..close].trim();
    let tail = rest[close + 1..].trim_start();
    if !tail.starts_with('{') {
        return None;
    }

    if params.starts_with("...{") {
        // Flow component params may use spread-object syntax:
        //   component C(...{scope = foo}: any) { ... }
        // JS `function` params cannot use a rest object pattern, so drop the spread
        // and keep the object binding/type annotation for downstream lowering.
        let rewritten_param = params.trim_start_matches("...");
        return Some(format!(
            "{}function {}({}) {}",
            prefix, name, rewritten_param, tail
        ));
    }

    if params.is_empty() || params.starts_with("...") {
        return None;
    }
    if params.contains(',') {
        let parts: Vec<&str> = params.split(',').map(str::trim).collect();
        if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        let (first_name, first_type) = if let Some(colon) = parts[0].find(':') {
            let name = parts[0][..colon].trim();
            let ty = parts[0][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[0], None)
        };
        let (second_name, second_type) = if let Some(colon) = parts[1].find(':') {
            let name = parts[1][..colon].trim();
            let ty = parts[1][colon + 1..].trim();
            (name, Some(ty))
        } else {
            (parts[1], None)
        };
        if second_name != "ref" || !is_valid_js_identifier(first_name) {
            return None;
        }

        let rewritten_props = if let Some(ty) = first_type {
            format!(
                "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
                first_name, first_name, ty
            )
        } else {
            format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", first_name, first_name)
        };
        let ref_param = if let Some(ty) = second_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        if is_export_prefixed {
            return None;
        }
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef({}, {}): React.Node {}",
            prefix, name, name, prefix, name, rewritten_props, ref_param, tail
        ));
    }
    let (param_name, param_type) = if let Some(colon) = params.find(':') {
        let name = params[..colon].trim();
        let ty = params[colon + 1..].trim();
        (name, Some(ty))
    } else {
        (params, None)
    };
    if !is_valid_js_identifier(param_name) {
        return None;
    }

    if param_name == "ref" {
        if is_export_prefixed {
            return None;
        }
        let ref_param = if let Some(ty) = param_type {
            format!("ref: {}", ty)
        } else {
            "ref".to_string()
        };
        return Some(format!(
            "{}const {} = React.forwardRef({}_withRef);\n{}function {}_withRef(_$$empty_props_placeholder$$: $ReadOnly<{{ }}>, {}): React.Node {}",
            prefix, name, name, prefix, name, ref_param, tail
        ));
    }

    let rewritten_param = if let Some(ty) = param_type {
        format!(
            "{{ {} }}: $ReadOnly<{{ {}: {} }}>",
            param_name, param_name, ty
        )
    } else {
        format!("{{ {} }}: $ReadOnly<{{ {}: any }}>", param_name, param_name)
    };
    Some(format!(
        "{}function {}({}): React.Node {}",
        prefix, name, rewritten_param, tail
    ))
}

/// Find the position of a Flow keyword (`component` or `hook`) at the start of a statement.
/// Returns the byte offset of the keyword, or None.
/// Handles: standalone keyword, or after `export`, `export default`.
fn find_flow_keyword(line: &str, keyword: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let offset = line.len() - trimmed.len();

    // Direct: `component Name(` or `hook useName(`
    if let Some(after) = trimmed.strip_prefix(keyword)
        && (after.starts_with(' ') || after.starts_with('\t'))
    {
        return Some(offset);
    }

    // After `export default`: `export default component Name(`
    if let Some(rest) = trimmed.strip_prefix("export") {
        let rest = rest.trim_start();
        if let Some(rest) = rest.strip_prefix("default") {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix(keyword)
                && (after.starts_with(' ') || after.starts_with('\t'))
            {
                let kw_offset = line.len() - rest.len();
                return Some(kw_offset);
            }
        }
        // After `export`: `export component Name(`
        let rest2 = rest;
        if let Some(after) = rest2.strip_prefix(keyword)
            && (after.starts_with(' ') || after.starts_with('\t'))
        {
            let kw_offset = line.len() - rest2.len();
            return Some(kw_offset);
        }
    }

    None
}

/// Count the number of unique reactive scopes that survive all pruning passes.
fn count_surviving_scopes(func: &crate::hir::types::HIRFunction) -> usize {
    let mut scope_ids = std::collections::HashSet::new();
    for (_, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let Some(scope) = &instr.lvalue.identifier.scope {
                scope_ids.insert(scope.id);
            }
        }
    }
    scope_ids.len()
}

fn collect_named_identifiers_hir(func: &HIRFunction) -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for param in &func.params {
        let place = match param {
            crate::hir::types::Argument::Place(place)
            | crate::hir::types::Argument::Spread(place) => place,
        };
        if let Some(name) = &place.identifier.name {
            names.insert(name.value().to_string());
        }
    }

    for place in &func.context {
        if let Some(name) = &place.identifier.name {
            names.insert(name.value().to_string());
        }
    }

    for (_, block) in &func.body.blocks {
        for phi in &block.phis {
            if let Some(name) = &phi.place.identifier.name {
                names.insert(name.value().to_string());
            }
        }
        for instr in &block.instructions {
            crate::hir::visitors::for_each_instruction_lvalue(instr, |place| {
                if let Some(name) = &place.identifier.name {
                    names.insert(name.value().to_string());
                }
            });
        }
    }

    names
}

/// Convert an outlined HIR function to rendered outlined metadata for emission.
///
/// Prefer the reactive codegen path for parity with nested context/capture handling.
/// Fall back to the legacy outlined emitter if reactive codegen reports an error.
fn codegen_outlined_function(
    func: &HIRFunction,
    enable_change_variable_codegen: bool,
    _reserved_names: &std::collections::HashSet<String>,
) -> Option<CompiledOutlinedFunction> {
    let (codegen, outlined_reactive_fn, own_unique_identifiers) = if matches!(
        func.fn_type,
        crate::hir::types::ReactFunctionType::Component
    ) {
        match run_hir_pipeline(
            func.clone(),
            func.id.as_deref().unwrap_or("<outlined>"),
            func.env.config(),
        ) {
            Ok(pipeline_output) if pipeline_output.codegen_result.error.is_none() => {
                let ui = pipeline_output.unique_identifiers.clone();
                (
                    pipeline_output.codegen_result,
                    Some(pipeline_output.reactive_function),
                    ui,
                )
            }
            _ => {
                let mut reactive_fn =
                    build_reactive_function::build_reactive_function(func.clone());
                prune_unused_labels_reactive::prune_unused_labels(&mut reactive_fn);
                prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn);
                if prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn).is_err() {
                    return None;
                }
                promote_used_temporaries::promote_used_temporaries_for_outlined(&mut reactive_fn);
                let unique_identifiers = rename_variables::rename_variables(
                    &mut reactive_fn,
                    enable_change_variable_codegen,
                    None,
                );
                let ui = unique_identifiers.clone();
                let alloc = oxc_allocator::Allocator::default();
                let bld = oxc_ast::AstBuilder::new(&alloc);
                let meta = crate::codegen_backend::codegen_ast::codegen_reactive_function(
                    bld,
                    &alloc,
                    &reactive_fn,
                    crate::codegen_backend::codegen_ast::CodegenOptions {
                        enable_change_variable_codegen,
                        enable_emit_hook_guards: false,
                        enable_change_detection_for_debugging: false,
                        enable_reset_cache_on_source_file_changes: false,
                        fast_refresh_source_hash: None,
                        disable_memoization_features: RETRY_NO_MEMO_MODE.with(|flag| flag.get()),
                        disable_memoization_for_debugging: false,
                        fbt_operands: Default::default(),
                        cache_binding_name: None,
                        unique_identifiers,
                        param_name_overrides: std::collections::HashMap::new(),
                        enable_name_anonymous_functions: false,
                    },
                )
                .metadata();
                if meta.error.is_some() {
                    return None;
                }
                (meta, Some(reactive_fn), ui)
            }
        }
    } else {
        let mut reactive_fn = build_reactive_function::build_reactive_function(func.clone());
        prune_unused_labels_reactive::prune_unused_labels(&mut reactive_fn);
        prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn);
        if prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn).is_err() {
            return None;
        }
        promote_used_temporaries::promote_used_temporaries_for_outlined(&mut reactive_fn);
        let unique_identifiers = rename_variables::rename_variables(
            &mut reactive_fn,
            enable_change_variable_codegen,
            None,
        );
        let ui = unique_identifiers.clone();
        let alloc = oxc_allocator::Allocator::default();
        let bld = oxc_ast::AstBuilder::new(&alloc);
        let meta = crate::codegen_backend::codegen_ast::codegen_reactive_function(
            bld,
            &alloc,
            &reactive_fn,
            crate::codegen_backend::codegen_ast::CodegenOptions {
                enable_change_variable_codegen,
                enable_emit_hook_guards: false,
                enable_change_detection_for_debugging: false,
                enable_reset_cache_on_source_file_changes: false,
                fast_refresh_source_hash: None,
                disable_memoization_features: RETRY_NO_MEMO_MODE.with(|flag| flag.get()),
                disable_memoization_for_debugging: false,
                fbt_operands: Default::default(),
                cache_binding_name: None,
                unique_identifiers,
                param_name_overrides: std::collections::HashMap::new(),
                enable_name_anonymous_functions: false,
            },
        )
        .metadata();
        if meta.error.is_some() {
            return None;
        }
        (meta, Some(reactive_fn), ui)
    };
    let rendered_params: Vec<CompiledParam> = func
        .params
        .iter()
        .enumerate()
        .map(|(index, param)| {
            let is_rest = matches!(param, crate::hir::types::Argument::Spread(_));
            let source_name = match param {
                crate::hir::types::Argument::Place(p) | crate::hir::types::Argument::Spread(p) => p
                    .identifier
                    .name
                    .as_ref()
                    .map(|n| n.value().to_string())
                    .unwrap_or_default(),
            };
            // Use param name from codegen (handles rename_variables renaming).
            let name = codegen
                .param_names
                .get(index)
                .cloned()
                .unwrap_or(source_name);
            CompiledParam { name, is_rest }
        })
        .collect();
    Some(CompiledOutlinedFunction {
        name: func.id.as_ref()?.clone(),
        params: rendered_params,
        directives: func.directives.iter().map(|d| format!("\"{d}\"")).collect(),
        cache_prologue: codegen.cache_prologue.clone(),
        needs_function_hook_guard_wrapper: codegen.needs_function_hook_guard_wrapper,
        is_async: func.async_,
        is_generator: func.generator,
        reactive_function: outlined_reactive_fn,
        unique_identifiers: own_unique_identifiers,
    })
}

fn outlined_function_needs_backend_render(
    outlined_function: &CompiledOutlinedFunction,
    hir_function: &HIRFunction,
) -> bool {
    let _ = hir_function;
    outlined_function.reactive_function.is_some()
}

fn dedupe_outlined_functions(outlined: &mut Vec<CompiledOutlinedFunction>) {
    let debug = std::env::var("DEBUG_OUTLINE_DEDUPE").is_ok();
    if debug {
        for (idx, outlined_function) in outlined.iter().enumerate() {
            eprintln!(
                "[OUTLINE_DEDUPE] before idx={} name={} params={:?}",
                idx, outlined_function.name, outlined_function.params
            );
        }
    }
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept_rev: Vec<CompiledOutlinedFunction> = Vec::with_capacity(outlined.len());
    for outlined_function in outlined.drain(..).rev() {
        if seen_names.insert(outlined_function.name.clone()) {
            kept_rev.push(outlined_function);
        } else if debug {
            eprintln!(
                "[OUTLINE_DEDUPE] drop-duplicate name={} params={:?}",
                outlined_function.name, outlined_function.params
            );
        }
    }
    kept_rev.reverse();
    *outlined = kept_rev;
}

fn dedupe_hir_outlined_functions(outlined: &mut Vec<(String, HIRFunction)>) {
    let debug = std::env::var("DEBUG_OUTLINE_DEDUPE").is_ok();
    if debug {
        for (idx, (name, hir_function)) in outlined.iter().enumerate() {
            eprintln!(
                "[OUTLINE_DEDUPE] before idx={} name={} hir_id={:?}",
                idx, name, hir_function.id
            );
        }
    }
    // Keep the *last* declaration for a given outlined function name.
    // This lets source-derived default-param outline bodies win over HIR
    // auto-outlines when both produce `_temp`.
    let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut kept_rev: Vec<(String, HIRFunction)> = Vec::with_capacity(outlined.len());
    for (name, hir_function) in outlined.drain(..).rev() {
        if seen_names.insert(name.clone()) {
            kept_rev.push((name, hir_function));
        } else if debug {
            eprintln!("[OUTLINE_DEDUPE] drop-duplicate name={}", name);
        }
    }
    kept_rev.reverse();
    *outlined = kept_rev;
}

// ---------------------------------------------------------------------------
// Shared pipeline: runs all HIR passes after lowering, returns codegen result
// ---------------------------------------------------------------------------

/// Result of running the HIR pipeline.
struct PipelineOutput {
    codegen_result: crate::codegen_backend::codegen_ast::CodegenMetadata,
    reactive_function: crate::hir::types::ReactiveFunction,
    final_hir_snapshot: HIRFunction,
    hir_outlined: Vec<optimization::outline_functions::OutlinedFunction>,
    reserved_removed_names: std::collections::HashSet<String>,
    has_fire_rewrite: bool,
    has_inferred_effect: bool,
    has_lower_context_access: bool,
    retry_no_memo_mode: bool,
    fbt_operands: std::collections::HashSet<crate::hir::types::IdentifierId>,
    unique_identifiers: std::collections::HashSet<String>,
}

/// Run the full HIR pipeline (from pruneMaybeThrows through codegen).
///
/// This matches upstream `runWithEnvironment()` pass ordering from Pipeline.ts.
/// Called from all three compile entry points (function, function_with_name, arrow).
/// Takes ownership of the HIRFunction because buildReactiveFunction consumes it.
fn run_hir_pipeline(
    mut hir_func: HIRFunction,
    name: &str,
    env_config: &crate::options::EnvironmentConfig,
) -> Result<PipelineOutput, crate::error::CompilerError> {
    let retry_no_memo_mode = RETRY_NO_MEMO_MODE.with(|flag| flag.get());
    // Note: validation errors in retry mode are logged but don't gate dememoization
    // (retry always dememoizes).
    let debug_pass = std::env::var("DEBUG_PASS").is_ok();
    macro_rules! trace_pass {
        ($name:expr) => {
            if debug_pass {
                eprintln!("[PASS] {}: {}", name, $name);
            }
        };
    }
    macro_rules! run_validation {
        ($expr:expr) => {
            if let Err(err) = $expr {
                if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                    eprintln!(
                        "[BAILOUT_REASON] fn={} stage=validation err={:?}",
                        name, err
                    );
                }
                if !retry_no_memo_mode {
                    return Err(err);
                }
                if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                    eprintln!("[RETRY_VALIDATION_IGNORED] {}: {:?}", name, err);
                }
            }
        };
    }

    // Test-only: throw unknown exception (upstream Pipeline.ts:562)
    if env_config.throw_unknown_exception_testonly {
        return Err(crate::error::CompilerError::Bail(crate::error::BailOut {
            reason: "unexpected error".to_string(),
            diagnostics: Vec::new(),
        }));
    }

    // -----------------------------------------------------------------------
    // Phase 0.5: Pre-validation checks on the raw HIR
    // -----------------------------------------------------------------------

    // Check for unsupported global function calls (matches upstream BuildHIR.ts:3681)
    run_validation!(validate_no_unsupported_global_calls(&hir_func));

    // -----------------------------------------------------------------------
    // Phase 1: HIR Pre-processing (before SSA)
    // -----------------------------------------------------------------------

    // pruneMaybeThrows (1st pass)
    prune_maybe_throws::prune_maybe_throws(&mut hir_func);

    // Validation — bail on error (return original source)
    run_validation!(
        validate_context_variable_lvalues::validate_context_variable_lvalues(&hir_func)
    );
    run_validation!(validate_use_memo::validate_use_memo(&hir_func));

    // Validate no void useMemo — upstream DropManualMemoization.ts line 447
    if env_config.validate_no_void_use_memo {
        run_validation!(validate_use_memo::validate_no_void_use_memo(&hir_func));
    }

    // Drop manual memoization (useMemo/useCallback) only when upstream gates allow it:
    // !enablePreserveExistingManualUseMemo &&
    // !disableMemoizationForDebugging &&
    // !enableChangeDetectionForDebugging
    if !env_config.enable_preserve_existing_manual_use_memo
        && !env_config.disable_memoization_for_debugging
        && !env_config.enable_change_detection_for_debugging
        && !retry_no_memo_mode
    {
        trace_pass!("drop_manual_memoization");
        drop_manual_memoization::drop_manual_memoization(&mut hir_func)?;
    }

    // Inline IIFEs (commonly created by drop_manual_memoization)
    trace_pass!("inline_iifes");
    inline_iifes::inline_iifes(&mut hir_func);

    // Merge consecutive blocks (simplifies CFG)
    trace_pass!("merge_consecutive_blocks");
    merge_consecutive_blocks::merge_consecutive_blocks(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
        maybe_dump_hir_blocks("after merge_consecutive_blocks", &hir_func);
    }

    // -----------------------------------------------------------------------
    // Phase 2: SSA + Analysis
    // -----------------------------------------------------------------------

    trace_pass!("enter_ssa");
    enter_ssa::enter_ssa(&mut hir_func)?;
    if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
        maybe_dump_hir_blocks("after enter_ssa", &hir_func);
    }
    trace_pass!("eliminate_redundant_phi");
    eliminate_redundant_phi::eliminate_redundant_phi(&mut hir_func);
    trace_pass!("constant_propagation");
    constant_propagation::constant_propagation(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
        maybe_dump_hir_blocks("after constant_propagation", &hir_func);
    }
    trace_pass!("infer_types");
    type_inference::infer_types(&mut hir_func);

    // Post-type-inference validation — bail on error
    run_validation!(validate_hooks_usage::validate_hooks_usage(&hir_func));
    if env_config.validate_no_capitalized_calls.is_some() {
        run_validation!(
            validate_no_capitalized_calls::validate_no_capitalized_calls(&hir_func, env_config,)
        );
    }

    // -----------------------------------------------------------------------
    // TransformFire (between type inference and mutation analysis)
    // -----------------------------------------------------------------------
    if env_config.enable_fire {
        trace_pass!("transform_fire");
        crate::hir::transform_fire::transform_fire(&mut hir_func)?;
    }

    let mut has_lower_context_access = false;
    if let Some(lowered_context_callee_config) = env_config.lower_context_access.as_ref() {
        trace_pass!("lower_context_access");
        has_lower_context_access = optimization::lower_context_access::lower_context_access(
            &mut hir_func,
            lowered_context_callee_config,
        );
        if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
            maybe_dump_hir_blocks("after lower_context_access", &hir_func);
        }
    }

    // -----------------------------------------------------------------------
    // Phase 3: Mutation/Aliasing Analysis
    // -----------------------------------------------------------------------

    trace_pass!("optimize_props_method_calls");
    optimization::optimize_props_method_calls::optimize_props_method_calls(&mut hir_func);

    trace_pass!("analyse_functions");
    analyse_functions::analyse_functions(&mut hir_func);

    // Port adaptation: captured context lowering for nested functions happens
    trace_pass!("infer_mutation_aliasing_effects");
    infer_mutation_aliasing_effects::infer_mutation_aliasing_effects(
        &mut hir_func,
        false,
        !retry_no_memo_mode,
    );

    // Outline functions after mutation/validation phases so nested function
    // side-effects are still visible to aliasing + validation passes.

    // Dead code elimination
    let pre_dce_named_identifiers = collect_named_identifiers_hir(&hir_func);
    trace_pass!("dead_code_elimination");
    dead_code_elimination::dead_code_elimination(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
        maybe_dump_hir_blocks("after dead_code_elimination", &hir_func);
    }

    if env_config.enable_instruction_reordering {
        trace_pass!("instruction_reordering");
        optimization::instruction_reordering::instruction_reordering(&mut hir_func);
        if std::env::var("DEBUG_HIR_BLOCKS_EARLY").is_ok() {
            maybe_dump_hir_blocks("after instruction_reordering", &hir_func);
        }
    }

    prune_maybe_throws::prune_maybe_throws(&mut hir_func);
    if let Err(err) =
        crate::inference::infer_mutation_aliasing_ranges::infer_mutation_aliasing_ranges(
            &mut hir_func,
            false,
        )
    {
        if !retry_no_memo_mode {
            return Err(err);
        }
        // Retry mode: validation failure logged but not used for dememoization
        // since retry always dememoizes.
        if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
            eprintln!("[RETRY_VALIDATION_IGNORED] {}: {:?}", name, err);
        }
    }

    // Post-aliasing validation — bail on error
    run_validation!(
        validate_locals_not_reassigned_after_render::validate_locals_not_reassigned_after_render(
            &hir_func,
        )
    );
    if env_config.validate_ref_access_during_render {
        run_validation!(
            validate_no_ref_access_in_render::validate_no_ref_access_in_render(&hir_func)
        );
    }
    if env_config.validate_no_set_state_in_render {
        run_validation!(
            validate_no_set_state_in_render::validate_no_set_state_in_render(&hir_func)
        );
    }
    if env_config.validate_no_derived_computations_in_effects {
        run_validation!(validate_no_derived_computations_in_effects::validate_no_derived_computations_in_effects(
            &hir_func
        ));
    }
    // Upstream uses env.logErrors() — errors are logged but do NOT bail compilation.
    if env_config.validate_no_set_state_in_effects {
        let _ = validate_no_set_state_in_effects::validate_no_set_state_in_effects(
            &hir_func,
            env_config.enable_allow_set_state_from_refs_in_effects,
        );
    }
    // Upstream uses env.logErrors() — errors are logged but do NOT bail compilation.
    if env_config.validate_no_jsx_in_try_statements {
        let _ = validate_no_jsx_in_try_statement::validate_no_jsx_in_try_statement(&hir_func);
    }
    if env_config.validate_no_impure_functions_in_render {
        run_validation!(
            validate_no_impure_functions_in_render::validate_no_impure_functions_in_render(
                &hir_func
            )
        );
    }
    // Unconditional — upstream Pipeline.ts line 290
    run_validation!(
        validate_no_freezing_known_mutable_functions::validate_no_freezing_known_mutable_functions(
            &hir_func
        )
    );

    // -----------------------------------------------------------------------
    // NEW PIPELINE (upstream ordering with reactive codegen):
    // -----------------------------------------------------------------------
    let _has_reactive = infer_reactive::infer_reactive_places(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after infer_reactive_places", &hir_func);
    }
    rewrite_instruction_kinds::rewrite_instruction_kinds(&mut hir_func)?;
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after rewrite_instruction_kinds", &hir_func);
    }

    // Validate static components — upstream Pipeline.ts line 304
    // Upstream uses env.logErrors() which logs but does NOT bail compilation.
    if env_config.validate_static_components {
        let _ = validate_static_components::validate_static_components(&hir_func);
    }

    let _scope_count =
        infer_scope_variables::infer_reactive_scope_variables_with_aliasing(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after infer_scope_variables_with_aliasing", &hir_func);
    }
    maybe_dump_identifier_scopes("after infer_scope_variables", &hir_func);
    // outline_functions already ran before DCE (shared section above)

    // MemoizeFbtAndMacroOperandsInSameScope: force fbt/fbs/macro operands into same scope
    let fbt_operands =
        memoize_fbt_operands::memoize_fbt_and_macro_operands_in_same_scope(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after memoize_fbt_operands_in_same_scope", &hir_func);
    }
    maybe_dump_identifier_scopes("after memoize_fbt_operands", &hir_func);

    // Upstream parity: outline JSX (optional) then function expressions.
    let mut hir_outlined = Vec::new();
    if env_config.enable_jsx_outlining {
        trace_pass!("outline_jsx");
        let jsx_outlined = optimization::outline_jsx::outline_jsx(&mut hir_func);
        if std::env::var("DEBUG_OUTLINE").is_ok() && !jsx_outlined.is_empty() {
            eprintln!(
                "[OUTLINE_JSX] {} — outlined {} functions: {:?}",
                name,
                jsx_outlined.len(),
                jsx_outlined
                    .iter()
                    .map(|f| f.name.clone())
                    .collect::<Vec<_>>()
            );
        }
        hir_outlined.extend(jsx_outlined);
        if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
            maybe_dump_hir_blocks("after outline_jsx", &hir_func);
        }
        maybe_dump_identifier_scopes("after outline_jsx", &hir_func);
    }

    if env_config.enable_name_anonymous_functions {
        trace_pass!("name_anonymous_functions");
        optimization::name_anonymous_functions::name_anonymous_functions(&mut hir_func);
        if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
            maybe_dump_hir_blocks("after name_anonymous_functions", &hir_func);
        }
        maybe_dump_identifier_scopes("after name_anonymous_functions", &hir_func);
    }

    if env_config.enable_function_outlining {
        trace_pass!("outline_functions");
        let fn_outlined =
            optimization::outline_functions::outline_functions(&mut hir_func, &fbt_operands);
        if std::env::var("DEBUG_OUTLINE").is_ok() && !fn_outlined.is_empty() {
            eprintln!(
                "[OUTLINE] {} — outlined {} functions: {:?}",
                name,
                fn_outlined.len(),
                fn_outlined
                    .iter()
                    .map(|f| f.name.clone())
                    .collect::<Vec<_>>()
            );
            for of in &fn_outlined {
                eprintln!(
                    "[OUTLINE_FUNC] name={} context_len={} blocks={}",
                    of.name,
                    of.func.context.len(),
                    of.func.body.blocks.len()
                );
                for (bid, block) in &of.func.body.blocks {
                    for instr in &block.instructions {
                        eprintln!(
                            "[OUTLINE_FUNC] name={} bb={} instr#{} lvalue=_t{} value={:?}",
                            of.name, bid.0, instr.id.0, instr.lvalue.identifier.id.0, instr.value
                        );
                    }
                    match &block.terminal {
                        crate::hir::types::Terminal::Return { value, .. } => {
                            let value_name = value.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", value.identifier.id.0),
                                |n| n.value().to_string(),
                            );
                            eprintln!(
                                "[OUTLINE_FUNC] name={} bb={} terminal=Return value={}#{}",
                                of.name, bid.0, value_name, value.identifier.id.0
                            );
                        }
                        other => {
                            eprintln!(
                                "[OUTLINE_FUNC] name={} bb={} terminal={:?}",
                                of.name, bid.0, other
                            );
                        }
                    }
                }
            }
        }
        hir_outlined.extend(fn_outlined);
    }

    // Scope alignment
    align_method_call_scopes::align_method_call_scopes(&mut hir_func);
    maybe_dump_identifier_scopes("after align_method_call_scopes", &hir_func);
    align_object_method_scopes::align_object_method_scopes(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after align_object_method_scopes", &hir_func);
    }
    maybe_dump_identifier_scopes("after align_object_method_scopes", &hir_func);
    prune_unused_labels::prune_unused_labels_hir(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after prune_unused_labels_hir", &hir_func);
    }
    maybe_dump_identifier_scopes("after prune_unused_labels", &hir_func);
    align_scopes::align_reactive_scopes_to_block_scopes(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after align_reactive_scopes_to_block_scopes", &hir_func);
    }
    maybe_dump_identifier_scopes("after align_scopes", &hir_func);
    merge_overlapping_scopes::merge_overlapping_reactive_scopes(&mut hir_func);
    if std::env::var("DEBUG_HIR_BLOCKS_TRACE").is_ok() {
        maybe_dump_hir_blocks("after merge_overlapping_reactive_scopes", &hir_func);
    }
    maybe_dump_identifier_scopes("after merge_overlapping_scopes", &hir_func);

    // Build scope terminals for reactive function tree
    maybe_dump_hir_blocks("before build_reactive_scope_terminals", &hir_func);
    build_reactive_scope_terminals::build_reactive_scope_terminals(&mut hir_func);
    maybe_dump_hir_scope_terminals("after build_reactive_scope_terminals", &hir_func);
    maybe_dump_hir_blocks("after build_reactive_scope_terminals", &hir_func);

    flatten_reactive_loops::flatten_reactive_loops_hir(&mut hir_func);
    maybe_dump_hir_scope_terminals("after flatten_reactive_loops", &hir_func);
    maybe_dump_hir_blocks("after flatten_reactive_loops", &hir_func);
    flatten_scopes_with_hooks::flatten_scopes_with_hooks_or_use_hir(&mut hir_func);
    maybe_dump_hir_scope_terminals("after flatten_scopes_with_hooks", &hir_func);
    maybe_dump_hir_blocks("after flatten_scopes_with_hooks", &hir_func);

    // Post-outline DCE: our outlining passes create dead locals that upstream's
    // BuildHIR avoids. TODO: fix outline_functions to not create dead locals,
    // then remove this pass.
    trace_pass!("dead_code_elimination_post_outline");
    dead_code_elimination::dead_code_elimination_post_outline(&mut hir_func);

    // New HIR-level dependency propagation (uses scope terminals)
    propagate_scope_dependencies_hir::propagate_scope_dependencies_hir(&mut hir_func);
    maybe_dump_hir_scope_terminals("after propagate_scope_dependencies_hir", &hir_func);
    maybe_dump_hir_blocks("after propagate_scope_dependencies_hir", &hir_func);

    if hir_func.env.config().infer_effect_dependencies.is_some() {
        let has_unresolved_effect_autodeps =
            infer_effect_dependencies::infer_effect_dependencies(&mut hir_func, retry_no_memo_mode);
        if has_unresolved_effect_autodeps {
            if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                eprintln!(
                    "[BAILOUT_REASON] fn={} stage=infer_effect_dependencies reason=unresolved_effect_autodeps",
                    name
                );
            }
            return Err(crate::error::CompilerError::Bail(crate::error::BailOut {
                reason: "Cannot infer dependencies of this effect. This will break your build!"
                    .to_string(),
                diagnostics: Vec::new(),
            }));
        }

        if !retry_no_memo_mode
            && infer_effect_dependencies::has_mutation_after_effect_dependency_use(&hir_func)
        {
            if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
                eprintln!(
                    "[BAILOUT_REASON] fn={} stage=infer_effect_dependencies reason=mutation_after_effect_dependency_use",
                    name
                );
            }
            return Err(crate::error::CompilerError::Bail(crate::error::BailOut {
                reason: "This value cannot be modified".to_string(),
                diagnostics: vec![crate::error::CompilerDiagnostic {
                    severity: crate::error::DiagnosticSeverity::InvalidReact,
                    message: "Modifying a value used previously in an effect function or as an effect dependency is not allowed. Consider moving the modification before calling useEffect()".to_string(),
                }],
            }));
        }
    }

    if let Some(inline_jsx_config) = hir_func.env.config().inline_jsx_transform.clone() {
        trace_pass!("inline_jsx_transform");
        optimization::inline_jsx_transform::inline_jsx_transform(&mut hir_func, &inline_jsx_config);
        maybe_dump_hir_blocks("after inline_jsx_transform", &hir_func);
    }

    // -----------------------------------------------------------------------
    // Phase 8: BuildReactiveFunction + Post-Reactive Passes + Codegen
    // -----------------------------------------------------------------------

    let scopes_remain = count_surviving_scopes(&hir_func);
    if std::env::var("DEBUG_SCOPES").is_ok() {
        eprintln!(
            "[SCOPES] {} — created={}, final={}",
            name, _scope_count, scopes_remain
        );
    }
    // Capture feature flags before consuming the HIR function.
    let has_fire_rewrite = hir_func.env.has_fire_rewrite();
    let has_inferred_effect = hir_func.env.has_inferred_effect();
    let final_hir_snapshot = hir_func.clone();
    let post_hir_named_identifiers = collect_named_identifiers_hir(&hir_func);
    let mut reserved_removed_names: std::collections::HashSet<String> = pre_dce_named_identifiers;
    reserved_removed_names.retain(|name| !post_hir_named_identifiers.contains(name));

    // New reactive codegen path: buildReactiveFunction + post-reactive passes + codegen.
    // Old codegen fallback has been removed to keep a single implementation path.
    // In retry-no-memo mode, always dememoize: the entire purpose of the retry is
    // to compile without memoization (matching upstream's noMemoize codegen path).
    let should_dememoize = retry_no_memo_mode;
    let (codegen_result, reactive_function, unique_identifiers) = run_reactive_passes(
        hir_func,
        retry_no_memo_mode,
        should_dememoize,
        env_config,
        &reserved_removed_names,
        &fbt_operands,
    )?;

    Ok(PipelineOutput {
        codegen_result,
        reactive_function,
        final_hir_snapshot,
        hir_outlined,
        reserved_removed_names,
        has_fire_rewrite,
        has_inferred_effect,
        has_lower_context_access,
        retry_no_memo_mode,
        fbt_operands,
        unique_identifiers,
    })
}

/// Count scopes in a reactive function's body.
fn count_reactive_scopes(body: &crate::hir::types::ReactiveBlock) -> usize {
    use crate::hir::types::{ReactiveStatement, ReactiveTerminal};
    let mut count = 0;
    for stmt in body {
        match stmt {
            ReactiveStatement::Scope(block) => {
                count += 1;
                count += count_reactive_scopes(&block.instructions);
            }
            ReactiveStatement::Terminal(term) => match &term.terminal {
                ReactiveTerminal::If {
                    consequent,
                    alternate,
                    ..
                } => {
                    count += count_reactive_scopes(consequent);
                    if let Some(alt) = alternate {
                        count += count_reactive_scopes(alt);
                    }
                }
                ReactiveTerminal::Switch { cases, .. } => {
                    for case in cases {
                        if let Some(block) = &case.block {
                            count += count_reactive_scopes(block);
                        }
                    }
                }
                ReactiveTerminal::For { loop_block, .. }
                | ReactiveTerminal::ForOf { loop_block, .. }
                | ReactiveTerminal::While { loop_block, .. }
                | ReactiveTerminal::DoWhile { loop_block, .. } => {
                    count += count_reactive_scopes(loop_block);
                }
                ReactiveTerminal::ForIn { loop_block, .. } => {
                    count += count_reactive_scopes(loop_block);
                }
                ReactiveTerminal::Label { block, .. } => {
                    count += count_reactive_scopes(block);
                }
                ReactiveTerminal::Try { block, handler, .. } => {
                    count += count_reactive_scopes(block);
                    count += count_reactive_scopes(handler);
                }
                _ => {}
            },
            _ => {}
        }
    }
    count
}

fn maybe_dump_reactive_scopes(label: &str, body: &crate::hir::types::ReactiveBlock) {
    if std::env::var("DEBUG_REACTIVE_SCOPES").is_err() {
        return;
    }
    eprintln!("[REACTIVE_SCOPES] {}", label);
    dump_reactive_scope_block(body);
}

fn maybe_dump_hir_scope_terminals(label: &str, hir: &crate::hir::types::HIRFunction) {
    if std::env::var("DEBUG_HIR_SCOPE_TERMINALS").is_err() {
        return;
    }
    use crate::hir::types::Terminal;

    eprintln!("[HIR_SCOPE_TERMINALS] {}", label);
    for (bid, block) in &hir.body.blocks {
        match &block.terminal {
            Terminal::Scope {
                block: scope_block,
                fallthrough,
                scope,
                id,
                ..
            } => {
                eprintln!(
                    "  bb{} term#{} scope={} range=({},{}) body=bb{} fallthrough=bb{} deps={} decls={} reassignments={}",
                    bid.0,
                    id.0,
                    scope.id.0,
                    scope.range.start.0,
                    scope.range.end.0,
                    scope_block.0,
                    fallthrough.0,
                    scope.dependencies.len(),
                    scope.declarations.len(),
                    scope.reassignments.len(),
                );
            }
            Terminal::PrunedScope {
                block: scope_block,
                fallthrough,
                scope,
                id,
                ..
            } => {
                eprintln!(
                    "  bb{} term#{} pruned-scope={} range=({},{}) body=bb{} fallthrough=bb{} deps={} decls={} reassignments={}",
                    bid.0,
                    id.0,
                    scope.id.0,
                    scope.range.start.0,
                    scope.range.end.0,
                    scope_block.0,
                    fallthrough.0,
                    scope.dependencies.len(),
                    scope.declarations.len(),
                    scope.reassignments.len(),
                );
            }
            _ => {}
        }
    }
}

fn maybe_dump_hir_blocks(label: &str, hir: &crate::hir::types::HIRFunction) {
    if std::env::var("DEBUG_HIR_BLOCKS").is_err() {
        return;
    }
    use crate::hir::types::Terminal;
    let debug_hir_instr = std::env::var("DEBUG_HIR_INSTR").is_ok();
    let debug_hir_instr_brief = std::env::var("DEBUG_HIR_INSTR_BRIEF").is_ok();
    let debug_hir_phi = std::env::var("DEBUG_HIR_PHI").is_ok();

    eprintln!("[HIR_BLOCKS] {}", label);
    for (bid, block) in &hir.body.blocks {
        let instr_count = block.instructions.len();
        match &block.terminal {
            Terminal::Return { .. } => {
                eprintln!("  bb{} instrs={} term=return", bid.0, instr_count);
            }
            Terminal::Throw { .. } => {
                eprintln!("  bb{} instrs={} term=throw", bid.0, instr_count);
            }
            Terminal::Goto { block, variant, .. } => {
                eprintln!(
                    "  bb{} instrs={} term=goto({:?}) -> bb{}",
                    bid.0, instr_count, variant, block.0
                );
            }
            Terminal::If {
                consequent,
                alternate,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=if cons=bb{} alt=bb{} ft=bb{}",
                    bid.0, instr_count, consequent.0, alternate.0, fallthrough.0
                );
            }
            Terminal::Branch {
                consequent,
                alternate,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=branch cons=bb{} alt=bb{}",
                    bid.0, instr_count, consequent.0, alternate.0
                );
            }
            Terminal::Switch { fallthrough, .. } => {
                eprintln!(
                    "  bb{} instrs={} term=switch ft=bb{}",
                    bid.0, instr_count, fallthrough.0
                );
            }
            Terminal::For {
                init,
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=for init=bb{} test=bb{} loop=bb{} ft=bb{}",
                    bid.0, instr_count, init.0, test.0, loop_block.0, fallthrough.0
                );
            }
            Terminal::ForOf {
                init,
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=for-of init=bb{} test=bb{} loop=bb{} ft=bb{}",
                    bid.0, instr_count, init.0, test.0, loop_block.0, fallthrough.0
                );
            }
            Terminal::ForIn {
                init,
                loop_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=for-in init=bb{} loop=bb{} ft=bb{}",
                    bid.0, instr_count, init.0, loop_block.0, fallthrough.0
                );
            }
            Terminal::While {
                test,
                loop_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=while test=bb{} loop=bb{} ft=bb{}",
                    bid.0, instr_count, test.0, loop_block.0, fallthrough.0
                );
            }
            Terminal::DoWhile {
                loop_block,
                test,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=do-while loop=bb{} test=bb{} ft=bb{}",
                    bid.0, instr_count, loop_block.0, test.0, fallthrough.0
                );
            }
            Terminal::Try {
                block,
                handler,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=try block=bb{} handler=bb{} ft=bb{}",
                    bid.0, instr_count, block.0, handler.0, fallthrough.0
                );
            }
            Terminal::Scope {
                block: scope_block,
                fallthrough,
                scope,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=scope({}) body=bb{} ft=bb{}",
                    bid.0, instr_count, scope.id.0, scope_block.0, fallthrough.0
                );
            }
            Terminal::PrunedScope {
                block: scope_block,
                fallthrough,
                scope,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=pruned-scope({}) body=bb{} ft=bb{}",
                    bid.0, instr_count, scope.id.0, scope_block.0, fallthrough.0
                );
            }
            Terminal::Label {
                block: label_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=label body=bb{} ft=bb{}",
                    bid.0, instr_count, label_block.0, fallthrough.0
                );
            }
            Terminal::Sequence {
                block: seq_block,
                fallthrough,
                ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=sequence block=bb{} ft=bb{}",
                    bid.0, instr_count, seq_block.0, fallthrough.0
                );
            }
            Terminal::Optional {
                test, fallthrough, ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=optional test=bb{} ft=bb{}",
                    bid.0, instr_count, test.0, fallthrough.0
                );
            }
            Terminal::Logical {
                test, fallthrough, ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=logical test=bb{} ft=bb{}",
                    bid.0, instr_count, test.0, fallthrough.0
                );
            }
            Terminal::Ternary {
                test, fallthrough, ..
            } => {
                eprintln!(
                    "  bb{} instrs={} term=ternary test=bb{} ft=bb{}",
                    bid.0, instr_count, test.0, fallthrough.0
                );
            }
            Terminal::Unreachable { .. } => {
                eprintln!("  bb{} instrs={} term=unreachable", bid.0, instr_count);
            }
            _ => {
                eprintln!("  bb{} instrs={} term=other", bid.0, instr_count);
            }
        }
        if debug_hir_phi && !block.phis.is_empty() {
            for phi in &block.phis {
                let phi_name = phi
                    .place
                    .identifier
                    .name
                    .as_ref()
                    .map(|name| name.value().to_string())
                    .unwrap_or_else(|| format!("_t{}", phi.place.identifier.id.0));
                eprintln!(
                    "    phi lvalue(id={},decl={},name={}) range=({}, {}) operands={}",
                    phi.place.identifier.id.0,
                    phi.place.identifier.declaration_id.0,
                    phi_name,
                    phi.place.identifier.mutable_range.start.0,
                    phi.place.identifier.mutable_range.end.0,
                    phi.operands.len()
                );
                let mut operands: Vec<_> = phi.operands.iter().collect();
                operands.sort_by_key(|(pred, _)| pred.0);
                for (pred, op) in operands {
                    let op_name = op
                        .identifier
                        .name
                        .as_ref()
                        .map(|name| name.value().to_string())
                        .unwrap_or_else(|| format!("_t{}", op.identifier.id.0));
                    eprintln!(
                        "      from bb{} -> id={} decl={} name={} range=({}, {})",
                        pred.0,
                        op.identifier.id.0,
                        op.identifier.declaration_id.0,
                        op_name,
                        op.identifier.mutable_range.start.0,
                        op.identifier.mutable_range.end.0
                    );
                }
            }
        }
        if debug_hir_instr {
            for instr in &block.instructions {
                eprintln!(
                    "    instr#{} lvalue(id={},decl={},name={:?}) {:?}",
                    instr.id.0,
                    instr.lvalue.identifier.id.0,
                    instr.lvalue.identifier.declaration_id.0,
                    instr.lvalue.identifier.name,
                    instr.value
                );
            }
        } else if debug_hir_instr_brief {
            for instr in &block.instructions {
                let lvalue_name = instr
                    .lvalue
                    .identifier
                    .name
                    .as_ref()
                    .map(|name| name.value().to_string())
                    .unwrap_or_else(|| format!("_t{}", instr.lvalue.identifier.id.0));
                let summary = match &instr.value {
                    crate::hir::types::InstructionValue::Primitive { value, .. } => {
                        format!("Primitive({value:?})")
                    }
                    crate::hir::types::InstructionValue::LoadLocal { place, .. } => format!(
                        "LoadLocal({})",
                        place.identifier.name.as_ref().map_or_else(
                            || format!("_t{}", place.identifier.id.0),
                            |name| name.value().to_string()
                        )
                    ),
                    crate::hir::types::InstructionValue::StoreLocal { lvalue, value, .. } => {
                        format!(
                            "StoreLocal({} <= _t{})",
                            lvalue.place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", lvalue.place.identifier.id.0),
                                |name| name.value().to_string()
                            ),
                            value.identifier.id.0
                        )
                    }
                    crate::hir::types::InstructionValue::LoadGlobal { binding, .. } => {
                        format!("LoadGlobal({})", binding.name())
                    }
                    crate::hir::types::InstructionValue::StoreGlobal { name, value, .. } => {
                        format!("StoreGlobal({name} <= _t{})", value.identifier.id.0)
                    }
                    crate::hir::types::InstructionValue::LoadContext { place, .. } => format!(
                        "LoadContext({})",
                        place.identifier.name.as_ref().map_or_else(
                            || format!("_t{}", place.identifier.id.0),
                            |name| name.value().to_string()
                        )
                    ),
                    crate::hir::types::InstructionValue::StoreContext { lvalue, value, .. } => {
                        format!(
                            "StoreContext({} <= _t{})",
                            lvalue.place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", lvalue.place.identifier.id.0),
                                |name| name.value().to_string()
                            ),
                            value.identifier.id.0
                        )
                    }
                    crate::hir::types::InstructionValue::FunctionExpression { .. } => {
                        "FunctionExpression".to_string()
                    }
                    crate::hir::types::InstructionValue::ObjectMethod { .. } => {
                        "ObjectMethod".to_string()
                    }
                    crate::hir::types::InstructionValue::CallExpression { callee, .. } => {
                        format!("CallExpression(callee=_t{})", callee.identifier.id.0)
                    }
                    crate::hir::types::InstructionValue::Destructure { .. } => {
                        "Destructure".to_string()
                    }
                    other => format!("{other:?}"),
                };
                eprintln!(
                    "    instr#{} lvalue={} (decl={}) {}",
                    instr.id.0, lvalue_name, instr.lvalue.identifier.declaration_id.0, summary
                );
            }
        }
    }
}

fn maybe_dump_identifier_scopes(label: &str, hir: &crate::hir::types::HIRFunction) {
    if std::env::var("DEBUG_SCOPE_RANGES").is_err() {
        return;
    }
    let mut by_scope: std::collections::HashMap<
        crate::hir::types::ScopeId,
        crate::hir::types::MutableRange,
    > = std::collections::HashMap::new();
    let mut visit_place = |place: &crate::hir::types::Place| {
        if let Some(scope) = &place.identifier.scope {
            by_scope
                .entry(scope.id)
                .or_insert_with(|| scope.range.clone());
        }
    };
    for (_, block) in &hir.body.blocks {
        for instr in &block.instructions {
            crate::hir::visitors::for_each_instruction_lvalue(instr, &mut visit_place);
            crate::hir::visitors::for_each_instruction_operand(instr, &mut visit_place);
        }
        crate::hir::visitors::for_each_terminal_operand(&block.terminal, &mut visit_place);
    }
    let mut parts = by_scope
        .iter()
        .map(|(id, range)| format!("{}:({},{})", id.0, range.start.0, range.end.0))
        .collect::<Vec<_>>();
    parts.sort();
    eprintln!("[SCOPE_RANGES] {label} {}", parts.join(", "));
}

fn dump_reactive_scope_block(body: &crate::hir::types::ReactiveBlock) {
    dump_reactive_scope_block_with_depth(body, 1);
}

fn dump_reactive_scope_block_with_depth(body: &crate::hir::types::ReactiveBlock, depth: usize) {
    use crate::hir::types::{ReactiveStatement, ReactiveTerminal};
    let indent = "  ".repeat(depth);
    let debug_reactive_instr = std::env::var("DEBUG_REACTIVE_INSTR").is_ok();
    let debug_reactive_instr_brief = std::env::var("DEBUG_REACTIVE_INSTR_BRIEF").is_ok();
    for stmt in body {
        match stmt {
            ReactiveStatement::Scope(scope_block) => {
                let scope = &scope_block.scope;
                let dep_names: Vec<String> = scope
                    .dependencies
                    .iter()
                    .map(|dep| {
                        dep.identifier.name.as_ref().map_or_else(
                            || format!("_t{}", dep.identifier.id.0),
                            |name| match name {
                                crate::hir::types::IdentifierName::Named(n)
                                | crate::hir::types::IdentifierName::Promoted(n) => n.clone(),
                            },
                        )
                    })
                    .collect();
                let decl_names: Vec<String> = scope
                    .declarations
                    .values()
                    .map(|decl| {
                        decl.identifier.name.as_ref().map_or_else(
                            || format!("_t{}", decl.identifier.id.0),
                            |name| match name {
                                crate::hir::types::IdentifierName::Named(n)
                                | crate::hir::types::IdentifierName::Promoted(n) => n.clone(),
                            },
                        )
                    })
                    .collect();
                eprintln!(
                    "{}scope={} deps={:?} decls={:?} reassignments={}",
                    indent,
                    scope.id.0,
                    dep_names,
                    decl_names,
                    scope.reassignments.len()
                );
                dump_reactive_scope_block_with_depth(&scope_block.instructions, depth + 1);
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                eprintln!("{}pruned-scope", indent);
                dump_reactive_scope_block_with_depth(&scope_block.instructions, depth + 1);
            }
            ReactiveStatement::Terminal(term_stmt) => match &term_stmt.terminal {
                ReactiveTerminal::If {
                    consequent,
                    alternate,
                    ..
                } => {
                    eprintln!(
                        "{}terminal if label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    dump_reactive_scope_block_with_depth(consequent, depth + 1);
                    if let Some(alt) = alternate {
                        dump_reactive_scope_block_with_depth(alt, depth + 1);
                    }
                }
                ReactiveTerminal::Switch { test, cases, .. } => {
                    eprintln!(
                        "{}terminal switch label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    if std::env::var("DEBUG_REACTIVE_INSTR").is_ok() {
                        eprintln!(
                            "{}terminal switch test(decl={}, name={:?}, id={})",
                            indent,
                            test.identifier.declaration_id.0,
                            test.identifier.name,
                            test.identifier.id.0
                        );
                    }
                    for case in cases {
                        if let Some(block) = &case.block {
                            dump_reactive_scope_block_with_depth(block, depth + 1);
                        }
                    }
                }
                ReactiveTerminal::For { loop_block, .. }
                | ReactiveTerminal::ForOf { loop_block, .. }
                | ReactiveTerminal::ForIn { loop_block, .. }
                | ReactiveTerminal::While { loop_block, .. }
                | ReactiveTerminal::DoWhile { loop_block, .. } => {
                    let kind = match &term_stmt.terminal {
                        ReactiveTerminal::For { .. } => "for",
                        ReactiveTerminal::ForOf { .. } => "for-of",
                        ReactiveTerminal::ForIn { .. } => "for-in",
                        ReactiveTerminal::While { .. } => "while",
                        ReactiveTerminal::DoWhile { .. } => "do-while",
                        _ => unreachable!(),
                    };
                    eprintln!(
                        "{}terminal {} label={:?}",
                        indent,
                        kind,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    if std::env::var("DEBUG_REACTIVE_INSTR").is_ok() {
                        match &term_stmt.terminal {
                            ReactiveTerminal::For { test, .. }
                            | ReactiveTerminal::ForOf { test, .. }
                            | ReactiveTerminal::While { test, .. }
                            | ReactiveTerminal::DoWhile { test, .. } => {
                                eprintln!(
                                    "{}terminal {:?} test(decl={}, name={:?}, id={})",
                                    indent,
                                    std::mem::discriminant(&term_stmt.terminal),
                                    test.identifier.declaration_id.0,
                                    test.identifier.name,
                                    test.identifier.id.0
                                );
                            }
                            ReactiveTerminal::ForIn { .. } => {
                                eprintln!(
                                    "{}terminal {:?}",
                                    indent,
                                    std::mem::discriminant(&term_stmt.terminal)
                                );
                            }
                            _ => {}
                        }
                    }
                    dump_reactive_scope_block_with_depth(loop_block, depth + 1);
                }
                ReactiveTerminal::Label { block, .. } => {
                    eprintln!(
                        "{}terminal label label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    dump_reactive_scope_block_with_depth(block, depth + 1);
                }
                ReactiveTerminal::Try { block, handler, .. } => {
                    eprintln!(
                        "{}terminal try label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    dump_reactive_scope_block_with_depth(block, depth + 1);
                    dump_reactive_scope_block_with_depth(handler, depth + 1);
                }
                ReactiveTerminal::Return { value, .. } => {
                    eprintln!(
                        "{}terminal return label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                    if debug_reactive_instr_brief {
                        let value_name = value
                            .identifier
                            .name
                            .as_ref()
                            .map(|name| name.value().to_string())
                            .unwrap_or_else(|| format!("_t{}", value.identifier.id.0));
                        eprintln!(
                            "{}  return value={} decl={} id={}",
                            indent,
                            value_name,
                            value.identifier.declaration_id.0,
                            value.identifier.id.0
                        );
                    }
                }
                ReactiveTerminal::Break { target, .. } => {
                    eprintln!(
                        "{}terminal break target=bb{} label={:?}",
                        indent,
                        target.0,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                }
                ReactiveTerminal::Continue { target, .. } => {
                    eprintln!(
                        "{}terminal continue target=bb{} label={:?}",
                        indent,
                        target.0,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                }
                ReactiveTerminal::Throw { .. } => {
                    eprintln!(
                        "{}terminal throw label={:?}",
                        indent,
                        term_stmt.label.as_ref().map(|l| l.id.0)
                    );
                }
            },
            ReactiveStatement::Instruction(instr) => {
                if debug_reactive_instr {
                    if let Some(lv) = &instr.lvalue {
                        eprintln!(
                            "{}instr#{} lvalue(decl={}, name={:?}) {:?}",
                            indent,
                            instr.id.0,
                            lv.identifier.declaration_id.0,
                            lv.identifier.name,
                            instr.value
                        );
                    } else {
                        eprintln!(
                            "{}instr#{} lvalue(none) {:?}",
                            indent, instr.id.0, instr.value
                        );
                    }
                } else if debug_reactive_instr_brief {
                    let lvalue_name = instr
                        .lvalue
                        .as_ref()
                        .and_then(|lv| {
                            lv.identifier
                                .name
                                .as_ref()
                                .map(|name| name.value().to_string())
                        })
                        .unwrap_or_else(|| {
                            instr
                                .lvalue
                                .as_ref()
                                .map(|lv| format!("_t{}", lv.identifier.id.0))
                                .unwrap_or_else(|| "<none>".to_string())
                        });
                    let summary = match &instr.value {
                        crate::hir::types::InstructionValue::Primitive { value, .. } => {
                            format!("Primitive({value:?})")
                        }
                        crate::hir::types::InstructionValue::LoadLocal { place, .. } => format!(
                            "LoadLocal({})",
                            place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", place.identifier.id.0),
                                |name| name.value().to_string()
                            )
                        ),
                        crate::hir::types::InstructionValue::StoreLocal {
                            lvalue, value, ..
                        } => format!(
                            "StoreLocal({} <= _t{})",
                            lvalue.place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", lvalue.place.identifier.id.0),
                                |name| name.value().to_string()
                            ),
                            value.identifier.id.0
                        ),
                        crate::hir::types::InstructionValue::LoadContext { place, .. } => format!(
                            "LoadContext({})",
                            place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", place.identifier.id.0),
                                |name| name.value().to_string()
                            )
                        ),
                        crate::hir::types::InstructionValue::StoreContext {
                            lvalue, value, ..
                        } => format!(
                            "StoreContext({} <= _t{})",
                            lvalue.place.identifier.name.as_ref().map_or_else(
                                || format!("_t{}", lvalue.place.identifier.id.0),
                                |name| name.value().to_string()
                            ),
                            value.identifier.id.0
                        ),
                        crate::hir::types::InstructionValue::LoadGlobal { binding, .. } => {
                            format!("LoadGlobal({})", binding.name())
                        }
                        crate::hir::types::InstructionValue::StoreGlobal {
                            name, value, ..
                        } => {
                            format!("StoreGlobal({name} <= _t{})", value.identifier.id.0)
                        }
                        crate::hir::types::InstructionValue::FunctionExpression { .. } => {
                            "FunctionExpression".to_string()
                        }
                        crate::hir::types::InstructionValue::ObjectMethod { .. } => {
                            "ObjectMethod".to_string()
                        }
                        crate::hir::types::InstructionValue::CallExpression { callee, .. } => {
                            format!("CallExpression(callee=_t{})", callee.identifier.id.0)
                        }
                        crate::hir::types::InstructionValue::Destructure { .. } => {
                            "Destructure".to_string()
                        }
                        other => format!("{other:?}"),
                    };
                    if let Some(lv) = &instr.lvalue {
                        eprintln!(
                            "{}instr#{} lvalue={} (decl={}) {}",
                            indent,
                            instr.id.0,
                            lvalue_name,
                            lv.identifier.declaration_id.0,
                            summary
                        );
                    } else {
                        eprintln!("{}instr#{} lvalue=<none> {}", indent, instr.id.0, summary);
                    }
                }
            }
        }
    }
}

/// Run buildReactiveFunction + all post-reactive passes + AST codegen.
fn run_reactive_passes(
    hir_func: HIRFunction,
    retry_no_memo_mode: bool,
    should_dememoize: bool,
    env_config: &crate::options::EnvironmentConfig,
    reserved_names: &std::collections::HashSet<String>,
    fbt_operands: &std::collections::HashSet<crate::hir::types::IdentifierId>,
) -> Result<
    (
        crate::codegen_backend::codegen_ast::CodegenMetadata,
        crate::hir::types::ReactiveFunction,
        std::collections::HashSet<String>,
    ),
    crate::error::CompilerError,
> {
    let debug = std::env::var("DEBUG_REACTIVE").is_ok();

    // Build reactive function tree from HIR CFG (consumes the HIRFunction).
    let mut reactive_fn = build_reactive_function::build_reactive_function(hir_func);
    maybe_dump_reactive_scopes("after build", &reactive_fn.body);

    if debug {
        let n = count_reactive_scopes(&reactive_fn.body);
        eprintln!("[REACTIVE] after build: {} scopes", n);
    }

    // Post-reactive passes (upstream ordering from Pipeline.ts):
    prune_unused_labels_reactive::prune_unused_labels(&mut reactive_fn);
    prune_non_escaping_scopes::prune_non_escaping_scopes_with_options(
        &mut reactive_fn,
        env_config.enable_preserve_existing_memoization_guarantees,
    );
    maybe_dump_reactive_scopes("after prune_non_escaping_scopes", &reactive_fn.body);

    if debug {
        let n = count_reactive_scopes(&reactive_fn.body);
        eprintln!("[REACTIVE] after prune_non_escaping: {} scopes", n);
    }

    prune_non_reactive_deps_reactive::prune_non_reactive_deps_reactive(&mut reactive_fn);
    prune_unused_scopes_reactive::prune_unused_scopes(&mut reactive_fn);
    maybe_dump_reactive_scopes("after prune_non_reactive/prune_unused", &reactive_fn.body);

    if debug {
        let n = count_reactive_scopes(&reactive_fn.body);
        eprintln!("[REACTIVE] after prune_unused: {} scopes", n);
    }

    merge_scopes_invalidate_together::merge_scopes_invalidate_together(&mut reactive_fn);
    prune_always_invalidating_reactive::prune_always_invalidating_scopes(&mut reactive_fn);
    maybe_dump_reactive_scopes("after merge/prune_invalidating", &reactive_fn.body);

    if debug {
        let n = count_reactive_scopes(&reactive_fn.body);
        eprintln!("[REACTIVE] after prune_invalidating: {} scopes", n);
    }

    if env_config.enable_change_detection_for_debugging {
        prune_initialization_dependencies::prune_initialization_dependencies(&mut reactive_fn);
        if debug {
            let n = count_reactive_scopes(&reactive_fn.body);
            eprintln!(
                "[REACTIVE] after prune_initialization_dependencies: {} scopes",
                n
            );
        }
    }

    if std::env::var("DEBUG_DISABLE_PROPAGATE_EARLY_RETURNS").is_err() {
        propagate_early_returns::propagate_early_returns(&mut reactive_fn);
    } else if debug {
        eprintln!(
            "[REACTIVE] skip propagate_early_returns (DEBUG_DISABLE_PROPAGATE_EARLY_RETURNS=1)"
        );
    }
    prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn);
    if !retry_no_memo_mode {
        promote_used_temporaries::promote_used_temporaries(&mut reactive_fn);
    }
    extract_scope_destructuring::extract_scope_destructuring(&mut reactive_fn);
    stabilize_block_ids::stabilize_block_ids(&mut reactive_fn);
    let unique_identifiers = rename_variables::rename_variables(
        &mut reactive_fn,
        env_config.enable_change_variable_codegen,
        Some(reserved_names),
    );
    prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn)?;
    maybe_dump_reactive_scopes("after final post-reactive passes", &reactive_fn.body);

    // Upstream Pipeline.ts line 537 — gated
    if env_config.validate_memoized_effect_dependencies {
        crate::validation::validate_memoized_effect_dependencies::validate_memoized_effect_dependencies(&reactive_fn)?;
    }

    // Upstream Pipeline.ts line 541-546 — gated
    let mut reactive_validation_error = false;
    if (env_config.enable_preserve_existing_memoization_guarantees
        || env_config.validate_preserve_existing_memoization_guarantees)
        && let Err(err) = crate::validation::validate_preserved_manual_memoization::validate_preserved_manual_memoization(&reactive_fn) {
            if !retry_no_memo_mode {
                return Err(err);
            }
            reactive_validation_error = true;
        }

    // Only dememoize if this specific function had a validation error that was
    // swallowed (either in HIR pipeline or reactive passes). Upstream behavior:
    // panicThreshold:"none" retries the failing function without memoization,
    // but successful functions keep their memoization.
    if should_dememoize || reactive_validation_error {
        dememoize_reactive_block(&mut reactive_fn.body);
    }

    // Codegen: pure AST path.
    let unique_identifiers_for_ast = unique_identifiers.clone();
    let mut codegen_result = {
        let alloc = oxc_allocator::Allocator::default();
        let bld = oxc_ast::AstBuilder::new(&alloc);
        let opts = crate::codegen_backend::codegen_ast::CodegenOptions {
            enable_change_variable_codegen: env_config.enable_change_variable_codegen,
            enable_emit_hook_guards: env_config.enable_emit_hook_guards,
            enable_change_detection_for_debugging: env_config.enable_change_detection_for_debugging,
            enable_reset_cache_on_source_file_changes: env_config
                .enable_reset_cache_on_source_file_changes
                .unwrap_or(false),
            fast_refresh_source_hash: get_fast_refresh_source_hash(),
            disable_memoization_features: retry_no_memo_mode,
            disable_memoization_for_debugging: env_config.disable_memoization_for_debugging,
            fbt_operands: fbt_operands.clone(),
            cache_binding_name: None,
            unique_identifiers: unique_identifiers_for_ast.clone(),
            param_name_overrides: std::collections::HashMap::new(),
            enable_name_anonymous_functions: env_config.enable_name_anonymous_functions,
        };
        crate::codegen_backend::codegen_ast::codegen_reactive_function(
            bld,
            &alloc,
            &reactive_fn,
            opts,
        )
        .metadata()
    };
    if let Some(err) = codegen_result.error.take() {
        return Err(err);
    }
    Ok((codegen_result, reactive_fn, unique_identifiers_for_ast))
}

fn dememoize_reactive_block(block: &mut crate::hir::types::ReactiveBlock) {
    use crate::hir::types::{PrunedReactiveScopeBlock, ReactiveStatement};

    for stmt in block.iter_mut() {
        let replacement = match stmt {
            ReactiveStatement::Scope(scope_block) => {
                dememoize_reactive_block(&mut scope_block.instructions);
                Some(ReactiveStatement::PrunedScope(PrunedReactiveScopeBlock {
                    scope: scope_block.scope.clone(),
                    instructions: std::mem::take(&mut scope_block.instructions),
                }))
            }
            ReactiveStatement::PrunedScope(scope_block) => {
                dememoize_reactive_block(&mut scope_block.instructions);
                None
            }
            ReactiveStatement::Terminal(term_stmt) => {
                dememoize_reactive_terminal(&mut term_stmt.terminal);
                None
            }
            ReactiveStatement::Instruction(_) => None,
        };
        if let Some(replacement) = replacement {
            *stmt = replacement;
        }
    }
}

fn dememoize_reactive_terminal(terminal: &mut crate::hir::types::ReactiveTerminal) {
    use crate::hir::types::ReactiveTerminal;

    match terminal {
        ReactiveTerminal::If {
            consequent,
            alternate,
            ..
        } => {
            dememoize_reactive_block(consequent);
            if let Some(alt) = alternate {
                dememoize_reactive_block(alt);
            }
        }
        ReactiveTerminal::Switch { cases, .. } => {
            for case in cases {
                if let Some(block) = &mut case.block {
                    dememoize_reactive_block(block);
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
            dememoize_reactive_block(loop_block);
        }
        ReactiveTerminal::Try { block, handler, .. } => {
            dememoize_reactive_block(block);
            dememoize_reactive_block(handler);
        }
        ReactiveTerminal::Break { .. }
        | ReactiveTerminal::Continue { .. }
        | ReactiveTerminal::Return { .. }
        | ReactiveTerminal::Throw { .. } => {}
    }
}

/// Extract directives from a function body that should be re-emitted in compiled output.
///
/// Even directives that affected compilation decisions, such as ignored opt-out directives,
/// still belong in the emitted function body when compilation proceeds.
fn extract_emitted_directives(body: &ast::FunctionBody<'_>) -> Vec<String> {
    body.directives
        .iter()
        .map(|d| format!("\"{}\"", d.expression.value.as_str()))
        .collect()
}

/// Check if a file already has the compiler runtime import (already compiled).
fn has_memo_cache_import(program: &ast::Program<'_>) -> bool {
    for stmt in &program.body {
        if let ast::Statement::ImportDeclaration(import) = stmt
            && (import.source.value.as_str() == "react/compiler-runtime"
                || import.source.value.as_str() == "react-compiler-runtime")
            && let Some(specifiers) = &import.specifiers
        {
            for specifier in specifiers {
                if let ast::ImportDeclarationSpecifier::ImportSpecifier(spec) = specifier {
                    let imported = match &spec.imported {
                        ast::ModuleExportName::IdentifierName(id) => id.name.as_str(),
                        ast::ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                        ast::ModuleExportName::StringLiteral(s) => s.value.as_str(),
                    };
                    if imported == "c" {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub(crate) struct RuntimeImportMergePlan {
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) replacement: Option<String>,
    pub(crate) merged_specs: Vec<(String, String)>,
    pub(crate) cache_local_name: Option<String>,
    pub(crate) has_cache_after: bool,
    pub(crate) has_use_fire_after: bool,
}

pub(crate) fn plan_runtime_import_merge(
    program: &ast::Program<'_>,
    needs_cache_import: bool,
    needs_fire_import: bool,
    cache_import_name: &str,
) -> Option<RuntimeImportMergePlan> {
    let mut found: Option<RuntimeImportMergePlan> = None;

    for stmt in &program.body {
        let ast::Statement::ImportDeclaration(import_decl) = stmt else {
            continue;
        };
        if import_decl.import_kind == ast::ImportOrExportKind::Type {
            continue;
        }
        if import_decl.source.value.as_str() != "react/compiler-runtime" {
            continue;
        }
        let Some(specifiers) = &import_decl.specifiers else {
            continue;
        };

        // Only merge into named-import forms:
        // import { ... } from "react/compiler-runtime"
        let mut merged_specs: Vec<(String, String)> = Vec::new();
        let mut has_cache = false;
        let mut has_use_fire = false;
        let mut cache_local_name: Option<String> = None;
        let mut can_merge = true;

        for specifier in specifiers {
            let ast::ImportDeclarationSpecifier::ImportSpecifier(spec) = specifier else {
                can_merge = false;
                break;
            };
            if spec.import_kind == ast::ImportOrExportKind::Type {
                continue;
            }
            let imported = match &spec.imported {
                ast::ModuleExportName::IdentifierName(id) => id.name.as_str(),
                ast::ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                ast::ModuleExportName::StringLiteral(s) => s.value.as_str(),
            };
            let local = spec.local.name.as_str();
            if imported == "c" {
                has_cache = true;
                cache_local_name = Some(local.to_string());
            } else if imported == "useFire" {
                has_use_fire = true;
            }
            merged_specs.push((imported.to_string(), local.to_string()));
        }

        if !can_merge {
            continue;
        }

        let mut changed = false;
        if needs_cache_import && !has_cache {
            merged_specs.push(("c".to_string(), cache_import_name.to_string()));
            has_cache = true;
            changed = true;
        }
        if needs_fire_import && !has_use_fire {
            merged_specs.push(("useFire".to_string(), "useFire".to_string()));
            has_use_fire = true;
            changed = true;
        }

        let replacement = if changed {
            let rendered_specs = merged_specs
                .iter()
                .map(|(imported, local)| {
                    if imported == local {
                        imported.clone()
                    } else {
                        format!("{imported} as {local}")
                    }
                })
                .collect::<Vec<_>>();
            Some(format!(
                "import {{ {} }} from \"react/compiler-runtime\";",
                rendered_specs.join(", ")
            ))
        } else {
            None
        };

        found = Some(RuntimeImportMergePlan {
            start: import_decl.span.start,
            end: import_decl.span.end,
            replacement,
            merged_specs,
            cache_local_name,
            has_cache_after: has_cache,
            has_use_fire_after: has_use_fire,
        });
        break;
    }

    found
}

/// Check if program-level directives opt out of compilation.
fn has_module_scope_opt_out(program: &ast::Program<'_>, custom_directives: &[String]) -> bool {
    for directive in &program.directives {
        let value = directive.expression.value.as_str();
        if OPT_OUT_DIRECTIVES.contains(&value) {
            return true;
        }
        if custom_directives.iter().any(|d| d == value) {
            return true;
        }
    }
    false
}

/// Check if a function body has an opt-out directive.
fn has_function_opt_out(body: &ast::FunctionBody<'_>, custom_directives: &[String]) -> bool {
    for directive in &body.directives {
        let value = directive.expression.value.as_str();
        if OPT_OUT_DIRECTIVES.contains(&value) {
            return true;
        }
        if custom_directives.iter().any(|d| d == value) {
            return true;
        }
    }
    false
}

/// Check if source has ESLint/Flow suppression comments for React rules.
///
/// Matches upstream's Suppression.ts: any suppression (eslint-disable, eslint-disable-next-line,
/// Flow $FlowFixMe[react-rule-*]) for react-hooks rules causes all functions to be skipped.
fn has_eslint_suppression(source: &str, options: &PluginOptions) -> bool {
    for line in source.lines() {
        let trimmed = line.trim();

        // Check eslint-disable (block) and eslint-disable-next-line (inline)
        if trimmed.contains("eslint-disable") {
            // Check standard react-hooks rules
            if trimmed.contains("react-hooks/rules-of-hooks")
                || trimmed.contains("react-hooks/exhaustive-deps")
            {
                return true;
            }
            // Check custom ESLint suppression rules
            if let Some(ref custom_rules) = options.eslint_suppression_rules {
                for rule in custom_rules {
                    if trimmed.contains(rule.as_str()) {
                        return true;
                    }
                }
            }
        }

        // Check Flow suppressions: $FlowFixMe[react-rule-*], $FlowExpectedError[react-rule-*], $FlowIssue[react-rule-*]
        if options.flow_suppressions
            && (trimmed.contains("$FlowFixMe")
                || trimmed.contains("$FlowExpectedError")
                || trimmed.contains("$FlowIssue"))
            && trimmed.contains("[react-rule")
        {
            return true;
        }
    }
    false
}

/// Determine the React function type: Component, Hook, or None (skip).
/// Port of `getReactFunctionType` + `getComponentOrHookLike` from upstream.
fn get_react_function_type(
    name: &str,
    body: &ast::FunctionBody<'_>,
    params: &ast::FormalParameters<'_>,
) -> Option<&'static str> {
    if is_component_name(name) {
        let bypass_param_validation = is_flow_component_name(name);
        // Component: must have hooks/JSX, valid params, doesn't return non-node.
        // NOTE: upstream does NOT require hooks/JSX, but without full reactive scope
        // pruning we produce false_memo for components without JSX. Keep this check
        // until pruneNonEscapingScopes is implemented.
        if calls_hooks_or_creates_jsx(body)
            && (bypass_param_validation || is_valid_component_params(params))
            && !returns_non_node(body)
        {
            return Some("Component");
        }
        return None;
    }
    if is_hook_name(name) {
        // Flow `hook` declarations are always treated as hooks regardless of body content,
        // matching the upstream Babel plugin where HookDeclaration AST nodes bypass the
        // hooks/JSX body check.
        if is_flow_hook_name(name) || calls_hooks_or_creates_jsx(body) {
            return Some("Hook");
        }
        return None;
    }
    None
}

/// Determine whether a function should be compiled based on compilation mode.
/// In `All` mode: compile everything (matching upstream's behavior of assigning 'Other' type).
/// In `Infer` mode: only components/hooks.
/// In `Annotation` mode: only functions with "use memo" directive.
fn should_compile_function(
    name: &str,
    body: &ast::FunctionBody<'_>,
    params: &ast::FormalParameters<'_>,
    mode: CompilationMode,
) -> bool {
    match mode {
        CompilationMode::All => true,
        CompilationMode::Infer => get_react_function_type(name, body, params).is_some(),
        CompilationMode::Annotation => {
            // Only compile if function has "use memo", "use memo if(...)" or "use forget" directive
            body.directives.iter().any(|d| {
                let v = d.expression.value.as_str();
                v == "use memo" || v == "use forget" || v.starts_with("use memo if(")
            })
        }
    }
}

/// Check if a name looks like a React component (starts with uppercase).
fn is_component_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Check if a name looks like a React hook (starts with "use" followed by uppercase or end).
fn is_hook_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("use") {
        rest.is_empty() || rest.chars().next().is_some_and(|c| c.is_uppercase())
    } else {
        false
    }
}

/// Validate component parameters (port of upstream isValidComponentParams):
/// - 0 params: valid
/// - 1 param: valid if not rest element and has valid props annotation
/// - 2 params: valid if first has valid props annotation AND second param name contains "ref"/"Ref"
/// - 3+ params: invalid
fn is_valid_component_params(params: &ast::FormalParameters<'_>) -> bool {
    let count = params.items.len();
    if count == 0 {
        return true;
    }
    if count > 2 {
        return false;
    }
    // Check first param has valid props annotation (reject primitive types)
    if let Some(first) = params.items.first()
        && !is_valid_props_annotation(first)
    {
        return false;
    }
    if count == 1 {
        // Single param: valid if not a rest element
        return params.rest.is_none() || !params.items.is_empty();
    }
    // 2 params: second must be ref-like identifier
    if let Some(second) = params.items.get(1) {
        if let ast::BindingPattern::BindingIdentifier(ident) = &second.pattern {
            let name = ident.name.as_str();
            return name.contains("ref") || name.contains("Ref");
        }
        return false;
    }
    false
}

/// Check if a parameter has a valid props type annotation.
/// Port of upstream's isValidPropsAnnotation — rejects primitive type annotations.
fn is_valid_props_annotation(param: &ast::FormalParameter<'_>) -> bool {
    use oxc_ast::ast::TSType;
    let Some(annot) = &param.type_annotation else {
        return true; // No annotation → valid (could be anything)
    };
    // Reject primitive/non-object types — these can't be React props
    !matches!(
        &annot.type_annotation,
        TSType::TSArrayType(_)
            | TSType::TSBigIntKeyword(_)
            | TSType::TSBooleanKeyword(_)
            | TSType::TSConstructorType(_)
            | TSType::TSFunctionType(_)
            | TSType::TSLiteralType(_)
            | TSType::TSNeverKeyword(_)
            | TSType::TSNumberKeyword(_)
            | TSType::TSStringKeyword(_)
            | TSType::TSSymbolKeyword(_)
            | TSType::TSTupleType(_)
    )
}

/// Check if function body contains hook calls or JSX creation (non-recursive).
fn calls_hooks_or_creates_jsx(body: &ast::FunctionBody<'_>) -> bool {
    // Use a simple visitor that checks top-level statements (not nested functions)
    for stmt in &body.statements {
        if stmt_has_hooks_or_jsx(stmt) {
            return true;
        }
    }
    false
}

fn stmt_has_hooks_or_jsx(stmt: &ast::Statement<'_>) -> bool {
    match stmt {
        ast::Statement::ExpressionStatement(expr) => expr_has_hooks_or_jsx(&expr.expression),
        ast::Statement::ReturnStatement(ret) => ret
            .argument
            .as_ref()
            .is_some_and(|e| expr_has_hooks_or_jsx(e)),
        ast::Statement::VariableDeclaration(decl) => decl
            .declarations
            .iter()
            .any(|d| d.init.as_ref().is_some_and(|e| expr_has_hooks_or_jsx(e))),
        ast::Statement::IfStatement(if_stmt) => {
            expr_has_hooks_or_jsx(&if_stmt.test)
                || stmt_has_hooks_or_jsx(&if_stmt.consequent)
                || if_stmt
                    .alternate
                    .as_ref()
                    .is_some_and(|a| stmt_has_hooks_or_jsx(a))
        }
        ast::Statement::BlockStatement(block) => {
            block.body.iter().any(|s| stmt_has_hooks_or_jsx(s))
        }
        ast::Statement::ForStatement(f) => stmt_has_hooks_or_jsx(&f.body),
        ast::Statement::ForOfStatement(f) => {
            expr_has_hooks_or_jsx(&f.right) || stmt_has_hooks_or_jsx(&f.body)
        }
        ast::Statement::ForInStatement(f) => {
            expr_has_hooks_or_jsx(&f.right) || stmt_has_hooks_or_jsx(&f.body)
        }
        ast::Statement::WhileStatement(w) => {
            expr_has_hooks_or_jsx(&w.test) || stmt_has_hooks_or_jsx(&w.body)
        }
        ast::Statement::DoWhileStatement(d) => {
            expr_has_hooks_or_jsx(&d.test) || stmt_has_hooks_or_jsx(&d.body)
        }
        ast::Statement::SwitchStatement(s) => {
            expr_has_hooks_or_jsx(&s.discriminant)
                || s.cases.iter().any(|c| {
                    c.test.as_ref().is_some_and(|t| expr_has_hooks_or_jsx(t))
                        || c.consequent.iter().any(|s| stmt_has_hooks_or_jsx(s))
                })
        }
        ast::Statement::TryStatement(t) => {
            t.block.body.iter().any(|s| stmt_has_hooks_or_jsx(s))
                || t.handler
                    .as_ref()
                    .is_some_and(|h| h.body.body.iter().any(|s| stmt_has_hooks_or_jsx(s)))
                || t.finalizer
                    .as_ref()
                    .is_some_and(|f| f.body.iter().any(|s| stmt_has_hooks_or_jsx(s)))
        }
        ast::Statement::ThrowStatement(t) => expr_has_hooks_or_jsx(&t.argument),
        ast::Statement::LabeledStatement(l) => stmt_has_hooks_or_jsx(&l.body),
        _ => false,
    }
}

fn expr_has_hooks_or_jsx(expr: &ast::Expression<'_>) -> bool {
    match expr {
        ast::Expression::JSXElement(_) | ast::Expression::JSXFragment(_) => true,
        ast::Expression::CallExpression(call) => {
            // Check if callee is a hook
            if callee_is_hook(&call.callee) {
                return true;
            }
            // Check callee for nested hooks (e.g., foo().useHook())
            if expr_has_hooks_or_jsx(&call.callee) {
                return true;
            }
            // Check args for hooks/JSX
            call.arguments.iter().any(|a| match a {
                ast::Argument::SpreadElement(s) => expr_has_hooks_or_jsx(&s.argument),
                _ => {
                    let e: &ast::Expression<'_> = unsafe { std::mem::transmute(a) };
                    expr_has_hooks_or_jsx(e)
                }
            })
        }
        ast::Expression::ConditionalExpression(cond) => {
            expr_has_hooks_or_jsx(&cond.test)
                || expr_has_hooks_or_jsx(&cond.consequent)
                || expr_has_hooks_or_jsx(&cond.alternate)
        }
        ast::Expression::LogicalExpression(log) => {
            expr_has_hooks_or_jsx(&log.left) || expr_has_hooks_or_jsx(&log.right)
        }
        ast::Expression::AssignmentExpression(assign) => expr_has_hooks_or_jsx(&assign.right),
        ast::Expression::SequenceExpression(seq) => {
            seq.expressions.iter().any(|e| expr_has_hooks_or_jsx(e))
        }
        ast::Expression::ParenthesizedExpression(p) => expr_has_hooks_or_jsx(&p.expression),
        ast::Expression::TSAsExpression(ts) => expr_has_hooks_or_jsx(&ts.expression),
        ast::Expression::TSSatisfiesExpression(ts) => expr_has_hooks_or_jsx(&ts.expression),
        ast::Expression::TSNonNullExpression(ts) => expr_has_hooks_or_jsx(&ts.expression),
        ast::Expression::TSTypeAssertion(ts) => expr_has_hooks_or_jsx(&ts.expression),
        ast::Expression::AwaitExpression(a) => expr_has_hooks_or_jsx(&a.argument),
        ast::Expression::ObjectExpression(obj) => obj.properties.iter().any(|prop| match prop {
            ast::ObjectPropertyKind::ObjectProperty(p) => {
                expr_has_hooks_or_jsx(&p.value)
                    || p.key
                        .as_expression()
                        .is_some_and(|k| expr_has_hooks_or_jsx(k))
            }
            ast::ObjectPropertyKind::SpreadProperty(s) => expr_has_hooks_or_jsx(&s.argument),
        }),
        ast::Expression::ArrayExpression(arr) => arr.elements.iter().any(|el| match el {
            ast::ArrayExpressionElement::SpreadElement(s) => expr_has_hooks_or_jsx(&s.argument),
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => {
                let e: &ast::Expression<'_> = unsafe { std::mem::transmute(el) };
                expr_has_hooks_or_jsx(e)
            }
        }),
        ast::Expression::NewExpression(new_expr) => {
            expr_has_hooks_or_jsx(&new_expr.callee)
                || new_expr.arguments.iter().any(|a| match a {
                    ast::Argument::SpreadElement(s) => expr_has_hooks_or_jsx(&s.argument),
                    _ => {
                        let e: &ast::Expression<'_> = unsafe { std::mem::transmute(a) };
                        expr_has_hooks_or_jsx(e)
                    }
                })
        }
        ast::Expression::TaggedTemplateExpression(tagged) => expr_has_hooks_or_jsx(&tagged.tag),
        ast::Expression::TemplateLiteral(tpl) => {
            tpl.expressions.iter().any(|e| expr_has_hooks_or_jsx(e))
        }
        ast::Expression::BinaryExpression(bin) => {
            expr_has_hooks_or_jsx(&bin.left) || expr_has_hooks_or_jsx(&bin.right)
        }
        ast::Expression::UnaryExpression(un) => expr_has_hooks_or_jsx(&un.argument),
        ast::Expression::UpdateExpression(_) => false,
        ast::Expression::StaticMemberExpression(m) => expr_has_hooks_or_jsx(&m.object),
        ast::Expression::ComputedMemberExpression(m) => {
            expr_has_hooks_or_jsx(&m.object) || expr_has_hooks_or_jsx(&m.expression)
        }
        ast::Expression::YieldExpression(y) => y
            .argument
            .as_ref()
            .is_some_and(|a| expr_has_hooks_or_jsx(a)),
        _ => false,
    }
}

/// Check if a call expression callee is a hook (useXxx or Xxx.useXxx).
fn callee_is_hook(callee: &ast::Expression<'_>) -> bool {
    match callee {
        ast::Expression::Identifier(id) => is_hook_name(id.name.as_str()),
        ast::Expression::StaticMemberExpression(member) => {
            is_hook_name(member.property.name.as_str())
        }
        _ => false,
    }
}

/// Check if a function returns a non-node value (object, function, class, etc.).
/// If it does, it's not a component.
fn returns_non_node(body: &ast::FunctionBody<'_>) -> bool {
    for stmt in &body.statements {
        if let ast::Statement::ReturnStatement(ret) = stmt
            && let Some(arg) = &ret.argument
            && is_non_node_expr(arg)
        {
            return true;
        }
    }
    false
}

fn is_non_node_expr(expr: &ast::Expression<'_>) -> bool {
    matches!(
        expr,
        ast::Expression::ObjectExpression(_)
            | ast::Expression::ArrowFunctionExpression(_)
            | ast::Expression::FunctionExpression(_)
            | ast::Expression::BigIntLiteral(_)
            | ast::Expression::ClassExpression(_)
            | ast::Expression::NewExpression(_)
    )
}

/// Check if a function expression is wrapped in forwardRef() or memo().
fn is_forwardref_or_memo_arg<'a>(init: &ast::Expression<'a>) -> Option<&'a ast::Expression<'a>> {
    if let ast::Expression::CallExpression(call) = init
        && (is_react_api_callee(&call.callee, "forwardRef")
            || is_react_api_callee(&call.callee, "memo"))
        && let Some(first_arg) = call.arguments.first()
    {
        if matches!(first_arg, ast::Argument::SpreadElement(_)) {
            return None;
        }

        // Safely get the expression from the argument
        let expr: &ast::Expression<'a> = unsafe { std::mem::transmute(first_arg) };
        return Some(expr);
    }
    None
}

/// Check if an expression is `forwardRef` or `React.forwardRef` (or memo variants).
fn is_react_api_callee(callee: &ast::Expression<'_>, name: &str) -> bool {
    match callee {
        ast::Expression::Identifier(id) => id.name.as_str() == name,
        ast::Expression::StaticMemberExpression(member) => {
            member.property.name.as_str() == name
                && matches!(&member.object, ast::Expression::Identifier(id) if id.name.as_str() == "React")
        }
        _ => false,
    }
}

/// Compile a single source file.
///
/// Parses the source, identifies compilable functions (components/hooks),
/// runs the compiler pipeline on each, and returns the transformed output.
pub fn compile(filename: &str, source: &str, options: &PluginOptions) -> CompileResult {
    // Initial pass runs in normal mode. panicThreshold:"none" retries failing
    // functions in no-memo mode (see try_compile_* helpers).
    let retry_no_memo_mode = false;
    RETRY_NO_MEMO_MODE.with(|flag| flag.set(retry_no_memo_mode));
    FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(false));
    CURRENT_FILENAME.with(|cell| *cell.borrow_mut() = filename.to_string());
    FLOW_COMPONENT_NAMES.with(|set| {
        *set.borrow_mut() = collect_flow_component_names(source);
    });
    FLOW_HOOK_NAMES.with(|set| {
        *set.borrow_mut() = collect_flow_hook_names(source);
    });
    let _fast_refresh_hash_guard = FastRefreshHashGuard;
    let fast_refresh_hash = if options
        .environment
        .enable_reset_cache_on_source_file_changes
        .unwrap_or(false)
    {
        Some(compute_fast_refresh_source_hash(source))
    } else {
        None
    };
    set_fast_refresh_source_hash(fast_refresh_hash);
    macro_rules! untransformed_result {
        ($code:expr) => {{
            return CompileResult {
                transformed: false,
                code: $code,
                map: None,
            };
        }};
    }

    if options.no_emit {
        untransformed_result!(source.to_string());
    }

    // Pre-process: convert Flow `component`/`hook` declarations to `function`
    let mut source_owned = preprocess_flow_syntax(source);
    source_owned = rewrite_flow_component_param_lists(&source_owned);
    let source_untransformed = source_owned.clone();
    let mut source = source_owned.as_str();

    let allocator = oxc_allocator::Allocator::default();

    // Phase 0: Parse the source file
    // Always enable JSX — React Compiler always processes React code
    let source_type = if filename.ends_with(".tsx") {
        oxc_span::SourceType::tsx()
    } else if filename.ends_with(".ts") {
        oxc_span::SourceType::ts().with_jsx(true)
    } else if filename.ends_with(".jsx") {
        oxc_span::SourceType::jsx()
    } else {
        oxc_span::SourceType::mjs().with_jsx(true)
    };

    let parser_ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    // If JS/JSX parsing has errors, try again as TypeScript (some upstream fixtures
    // use TypeScript syntax in .js files, e.g. type annotations on parameters).
    // If that also fails, do a best-effort Flow signature type stripping fallback.
    let (program, source_type_used) = if !parser_ret.errors.is_empty()
        && !source_type.is_typescript()
    {
        let ts_type = oxc_span::SourceType::tsx();
        let allocator2 = oxc_allocator::Allocator::default();
        let ts_ret = oxc_parser::Parser::new(&allocator2, source, ts_type).parse();
        if ts_ret.panicked || !ts_ret.errors.is_empty() {
            // Try a final fallback by stripping Flow function signature annotations.
            let stripped_signatures = strip_flow_function_signature_types(source);
            let stripped_casts = rewrite_flow_cast_expressions(&stripped_signatures);
            let stripped = rewrite_flow_component_param_lists(&stripped_casts);
            if std::env::var("DEBUG_FLOW_PARSE").is_ok() {
                eprintln!(
                    "[FLOW_PARSE] initial js_errs={} ts_errs={} stripped_changed={} cast_rewrite_changed={}",
                    parser_ret.errors.len(),
                    ts_ret.errors.len(),
                    stripped != source,
                    stripped_casts != stripped_signatures
                );
            }
            if stripped != source {
                let stripped_js =
                    oxc_parser::Parser::new(&allocator, stripped.as_str(), source_type).parse();
                if std::env::var("DEBUG_FLOW_PARSE").is_ok() {
                    eprintln!(
                        "[FLOW_PARSE] stripped_js panicked={} errs={}",
                        stripped_js.panicked,
                        stripped_js.errors.len()
                    );
                    if !stripped_js.errors.is_empty() {
                        eprintln!("[FLOW_PARSE] stripped source:\n{}", stripped);
                    }
                }
                if !stripped_js.panicked && stripped_js.errors.is_empty() {
                    source_owned = stripped;
                    source = source_owned.as_str();
                    let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
                    (parsed.program, source_type)
                } else {
                    let stripped_ts =
                        oxc_parser::Parser::new(&allocator, stripped.as_str(), ts_type).parse();
                    if std::env::var("DEBUG_FLOW_PARSE").is_ok() {
                        eprintln!(
                            "[FLOW_PARSE] stripped_ts panicked={} errs={}",
                            stripped_ts.panicked,
                            stripped_ts.errors.len()
                        );
                    }
                    if !stripped_ts.panicked && stripped_ts.errors.is_empty() {
                        source_owned = stripped;
                        source = source_owned.as_str();
                        let parsed = oxc_parser::Parser::new(&allocator, source, ts_type).parse();
                        (parsed.program, ts_type)
                    } else {
                        if parser_ret.panicked {
                            untransformed_result!(source.to_string());
                        }
                        (parser_ret.program, source_type)
                    }
                }
            } else {
                // TS parsing also failed — fall back to original parse result.
                if parser_ret.panicked {
                    untransformed_result!(source.to_string());
                }
                (parser_ret.program, source_type)
            }
        } else {
            // TS parsing succeeded — but we need it in the original allocator's lifetime
            // Since we can't move across allocators easily, just re-parse with TS type
            // using the original allocator
            drop(ts_ret);
            drop(allocator2);
            let ts_ret2 = oxc_parser::Parser::new(&allocator, source, ts_type).parse();
            (ts_ret2.program, ts_type)
        }
    } else {
        if parser_ret.panicked {
            untransformed_result!(source.to_string());
        }
        (parser_ret.program, source_type)
    };

    crate::source_lines::set_current_source(source);

    // File-level skip: already compiled (has runtime import)
    if has_memo_cache_import(&program) {
        untransformed_result!(source.to_string());
    }

    // Module-level opt-out: 'use no memo' / 'use no forget' / custom directives
    if !options.ignore_use_no_forget
        && has_module_scope_opt_out(&program, &options.custom_opt_out_directives)
    {
        untransformed_result!(source.to_string());
    }

    // Scan for eslint/flow suppression comments that prevent compilation.
    // Upstream allows panicThreshold:"none" retry mode to continue for
    // fire/effect-inference passes even when suppressions exist.
    if has_eslint_suppression(source, options)
        && !(options.panic_threshold == PanicThreshold::None
            && (options.environment.enable_fire
                || options.environment.infer_effect_dependencies.is_some()))
    {
        untransformed_result!(source.to_string());
    }

    // Validate config: disableMemoizationForDebugging and enableChangeDetectionForDebugging
    // cannot be used together (upstream Environment.ts:753-763).
    if options.environment.disable_memoization_for_debugging
        && options.environment.enable_change_detection_for_debugging
    {
        untransformed_result!(source.to_string());
    }

    // Validate blocklisted imports: if any import statement references a blocklisted
    // module, bail out (upstream Imports.ts:validateRestrictedImports).
    if let Some(ref blocklisted) = options.environment.validate_blocklisted_imports
        && !blocklisted.is_empty()
    {
        for stmt in &program.body {
            if let oxc_ast::ast::Statement::ImportDeclaration(import_decl) = stmt {
                let module_name = import_decl.source.value.as_str();
                if blocklisted.iter().any(|b| b == module_name) {
                    untransformed_result!(source.to_string());
                }
            }
        }
    }

    // Validate no dynamically created components or hooks (upstream Program.ts:517).
    // This runs on ALL functions in the program, not just compilable ones.
    if options
        .environment
        .validate_no_dynamically_created_components_or_hooks
        && validate_no_dynamic_components_or_hooks_program(&program).is_err()
    {
        untransformed_result!(source.to_string());
    }

    // Build semantic analysis
    let semantic_ret = oxc_semantic::SemanticBuilder::new().build(&program);
    let semantic = semantic_ret.semantic;

    // Identify compilable functions and compile each
    let mut compiled = Vec::new();
    let custom_dirs = &options.custom_opt_out_directives;
    let ignore_opt_out = options.ignore_use_no_forget;
    for stmt in &program.body {
        collect_compilable_functions(
            stmt,
            source,
            &semantic,
            options,
            custom_dirs,
            ignore_opt_out,
            &mut compiled,
        );
    }

    if FILE_HAD_PIPELINE_ERROR.with(|flag| flag.get()) {
        if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
            eprintln!(
                "[PIPELINE_FILE_BAILOUT] file={} reason=function_error",
                filename
            );
        }
        untransformed_result!(source.to_string());
    }

    if compiled.is_empty() {
        untransformed_result!(source_untransformed);
    }

    // Dynamic gating: parse directive `use memo if(<identifier>)`.
    // Invalid/multiple directives should bail out without compilation.
    let dynamic_gate_ident = if options.dynamic_gating.is_some() {
        if options
            .environment
            .validate_preserve_existing_memoization_guarantees
        {
            untransformed_result!(source.to_string());
        }
        match parse_dynamic_gating_identifier(source) {
            Some(ident) => Some(ident),
            None => {
                untransformed_result!(source.to_string());
            }
        }
    } else {
        None
    };

    // Upstream currently bails out for infer-effect-deps fixtures that combine
    // gating with AUTODEPS in the non-Forget path (see infer-effect-deps
    // bailout-retry TODO fixtures). Match that behavior by skipping compilation.
    if options.environment.infer_effect_dependencies.is_some()
        && (options.gating.is_some() || dynamic_gate_ident.is_some())
        && source.contains("AUTODEPS")
    {
        untransformed_result!(source.to_string());
    }

    // Upstream currently bails out for AUTODEPS on default-import module property calls
    // (e.g. `React.useEffect(..., React.AUTODEPS)`), where effect inference cannot
    // reliably resolve dependencies in this path.
    if has_infer_effect_autodeps_default_import_property_call(&program, source, options) {
        untransformed_result!(source.to_string());
    }

    // Upstream currently errors for nested fbt/fbs calls inside `fbt.param`/`fbs.param`
    // values (known TODO fixture: `error.todo-fbt-param-nested-fbt`).
    // Match parity by bailing out at file level for this structural pattern.
    if has_nested_fbt_call_in_param_value(&program) {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[PIPELINE_FILE_BAILOUT] file={} reason=fbt_nested_param_call_value",
                filename
            );
        }
        untransformed_result!(source.to_string());
    }

    crate::codegen_backend::emit_module(
        ModuleEmitArgs {
            filename,
            source,
            source_untransformed: &source_untransformed,
            source_type: source_type_used,
            program: &program,
            options,
            dynamic_gate_ident: dynamic_gate_ident.as_deref(),
        },
        compiled,
    )
}

pub(crate) fn strip_single_param_arrow_parens_for_transform_flag(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'(' {
            let start = i + 1;
            if start < bytes.len() && is_ident_start_byte_for_transform_flag(bytes[start]) {
                let mut j = start + 1;
                while j < bytes.len() && is_ident_continue_byte_for_transform_flag(bytes[j]) {
                    j += 1;
                }
                if j + 2 < bytes.len()
                    && bytes[j] == b')'
                    && bytes[j + 1] == b'='
                    && bytes[j + 2] == b'>'
                {
                    out.push_str(&input[start..j]);
                    out.push_str("=>");
                    i = j + 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

pub(crate) fn strip_trailing_commas_before_closer_for_transform_flag(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b','
            && i + 1 < bytes.len()
            && (bytes[i + 1] == b'}' || bytes[i + 1] == b']' || bytes[i + 1] == b')')
        {
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn is_ident_start_byte_for_transform_flag(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || (byte as char).is_ascii_alphabetic()
}

fn is_ident_continue_byte_for_transform_flag(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || (byte as char).is_ascii_alphanumeric()
}

pub(crate) fn should_preserve_leading_body_statement(stmt: &ast::Statement<'_>) -> bool {
    matches!(stmt, ast::Statement::TSEnumDeclaration(_))
}

fn is_identifier_char(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphanumeric()
}

pub(crate) fn collect_top_level_bindings(
    program: &ast::Program<'_>,
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in &program.body {
        collect_top_level_statement_bindings(stmt, &mut names);
    }
    names
}

fn has_infer_effect_autodeps_default_import_property_call(
    program: &ast::Program<'_>,
    source: &str,
    options: &PluginOptions,
) -> bool {
    let Some(configs) = options.environment.infer_effect_dependencies.as_ref() else {
        return false;
    };

    let mut module_to_functions: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
    for config in configs {
        if config.function_name != "default" {
            module_to_functions
                .entry(config.function_module.clone())
                .or_default()
                .insert(config.function_name.clone());
        }
    }
    if module_to_functions.is_empty() {
        return false;
    }

    for stmt in &program.body {
        let ast::Statement::ImportDeclaration(import_decl) = stmt else {
            continue;
        };
        if import_decl.import_kind == ast::ImportOrExportKind::Type {
            continue;
        }
        let module_name = import_decl.source.value.as_str();
        let Some(function_names) = module_to_functions.get(module_name) else {
            continue;
        };
        let Some(specifiers) = &import_decl.specifiers else {
            continue;
        };

        for specifier in specifiers {
            let ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(default_spec) = specifier
            else {
                continue;
            };
            let local_name = default_spec.local.name.as_str();
            if !source.contains(&format!("{local_name}.AUTODEPS")) {
                continue;
            }
            if function_names
                .iter()
                .any(|fn_name| source.contains(&format!("{local_name}.{fn_name}(")))
            {
                return true;
            }
        }
    }

    false
}

type NameSet = std::collections::HashSet<String>;
type CallMatcher = fn(&ast::CallExpression<'_>, &NameSet) -> bool;

fn has_nested_fbt_call_in_param_value(program: &ast::Program<'_>) -> bool {
    let fbt_bindings = collect_fbt_module_bindings(program);
    if fbt_bindings.is_empty() {
        return false;
    }
    program.body.iter().any(|stmt| {
        stmt_has_call_match(stmt, &fbt_bindings, is_fbt_param_call_with_nested_fbt_value)
    })
}

fn collect_fbt_module_bindings(program: &ast::Program<'_>) -> NameSet {
    let mut bindings = NameSet::new();
    for stmt in &program.body {
        let ast::Statement::ImportDeclaration(import_decl) = stmt else {
            continue;
        };
        if import_decl.import_kind == ast::ImportOrExportKind::Type {
            continue;
        }
        if !matches!(import_decl.source.value.as_str(), "fbt" | "fbs") {
            continue;
        }
        let Some(specifiers) = &import_decl.specifiers else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                    bindings.insert(spec.local.name.as_str().to_string());
                }
                ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                    bindings.insert(spec.local.name.as_str().to_string());
                }
                ast::ImportDeclarationSpecifier::ImportSpecifier(spec)
                    if spec.imported.name() == "default" =>
                {
                    bindings.insert(spec.local.name.as_str().to_string());
                }
                _ => {}
            }
        }
    }
    bindings
}

fn stmt_has_call_match(
    stmt: &ast::Statement<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match stmt {
        ast::Statement::ExpressionStatement(expr_stmt) => {
            expr_has_call_match(&expr_stmt.expression, fbt_bindings, matcher)
        }
        ast::Statement::ReturnStatement(ret) => ret
            .argument
            .as_ref()
            .is_some_and(|arg| expr_has_call_match(arg, fbt_bindings, matcher)),
        ast::Statement::VariableDeclaration(decl) => decl
            .declarations
            .iter()
            .filter_map(|d| d.init.as_ref())
            .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
        ast::Statement::BlockStatement(block) => block
            .body
            .iter()
            .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher)),
        ast::Statement::IfStatement(if_stmt) => {
            expr_has_call_match(&if_stmt.test, fbt_bindings, matcher)
                || stmt_has_call_match(&if_stmt.consequent, fbt_bindings, matcher)
                || if_stmt
                    .alternate
                    .as_ref()
                    .is_some_and(|alt| stmt_has_call_match(alt, fbt_bindings, matcher))
        }
        ast::Statement::ForStatement(for_stmt) => {
            for_stmt.init.as_ref().is_some_and(|init| match init {
                ast::ForStatementInit::VariableDeclaration(decl) => decl
                    .declarations
                    .iter()
                    .filter_map(|d| d.init.as_ref())
                    .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
                _ => init
                    .as_expression()
                    .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
            }) || for_stmt
                .test
                .as_ref()
                .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher))
                || for_stmt
                    .update
                    .as_ref()
                    .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher))
                || stmt_has_call_match(&for_stmt.body, fbt_bindings, matcher)
        }
        ast::Statement::ForInStatement(for_in) => {
            expr_has_call_match(&for_in.right, fbt_bindings, matcher)
                || stmt_has_call_match(&for_in.body, fbt_bindings, matcher)
        }
        ast::Statement::ForOfStatement(for_of) => {
            expr_has_call_match(&for_of.right, fbt_bindings, matcher)
                || stmt_has_call_match(&for_of.body, fbt_bindings, matcher)
        }
        ast::Statement::WhileStatement(while_stmt) => {
            expr_has_call_match(&while_stmt.test, fbt_bindings, matcher)
                || stmt_has_call_match(&while_stmt.body, fbt_bindings, matcher)
        }
        ast::Statement::DoWhileStatement(do_while) => {
            stmt_has_call_match(&do_while.body, fbt_bindings, matcher)
                || expr_has_call_match(&do_while.test, fbt_bindings, matcher)
        }
        ast::Statement::SwitchStatement(switch_stmt) => {
            expr_has_call_match(&switch_stmt.discriminant, fbt_bindings, matcher)
                || switch_stmt.cases.iter().any(|case| {
                    case.test
                        .as_ref()
                        .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher))
                        || case
                            .consequent
                            .iter()
                            .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher))
                })
        }
        ast::Statement::TryStatement(try_stmt) => {
            try_stmt
                .block
                .body
                .iter()
                .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher))
                || try_stmt.handler.as_ref().is_some_and(|handler| {
                    handler
                        .body
                        .body
                        .iter()
                        .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher))
                })
                || try_stmt.finalizer.as_ref().is_some_and(|finalizer| {
                    finalizer
                        .body
                        .iter()
                        .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher))
                })
        }
        ast::Statement::ThrowStatement(throw_stmt) => {
            expr_has_call_match(&throw_stmt.argument, fbt_bindings, matcher)
        }
        ast::Statement::LabeledStatement(labeled) => {
            stmt_has_call_match(&labeled.body, fbt_bindings, matcher)
        }
        ast::Statement::FunctionDeclaration(func) => func
            .body
            .as_ref()
            .is_some_and(|body| body_has_call_match(body, fbt_bindings, matcher)),
        ast::Statement::ExportNamedDeclaration(export_decl) => export_decl
            .declaration
            .as_ref()
            .is_some_and(|decl| decl_has_call_match(decl, fbt_bindings, matcher)),
        ast::Statement::ExportDefaultDeclaration(export_decl) => {
            export_default_has_call_match(export_decl, fbt_bindings, matcher)
        }
        _ => false,
    }
}

fn decl_has_call_match(
    decl: &ast::Declaration<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match decl {
        ast::Declaration::VariableDeclaration(var_decl) => var_decl
            .declarations
            .iter()
            .filter_map(|d| d.init.as_ref())
            .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
        ast::Declaration::FunctionDeclaration(func) => func
            .body
            .as_ref()
            .is_some_and(|body| body_has_call_match(body, fbt_bindings, matcher)),
        _ => false,
    }
}

fn export_default_has_call_match(
    export_decl: &ast::ExportDefaultDeclaration<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match &export_decl.declaration {
        ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) => func
            .body
            .as_ref()
            .is_some_and(|body| body_has_call_match(body, fbt_bindings, matcher)),
        ast::ExportDefaultDeclarationKind::FunctionExpression(func) => func
            .body
            .as_ref()
            .is_some_and(|body| body_has_call_match(body, fbt_bindings, matcher)),
        ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
            body_has_call_match(&arrow.body, fbt_bindings, matcher)
        }
        ast::ExportDefaultDeclarationKind::CallExpression(call_expr) => {
            matcher(call_expr, fbt_bindings)
                || expr_has_call_match(&call_expr.callee, fbt_bindings, matcher)
                || call_expr
                    .arguments
                    .iter()
                    .any(|arg| arg_has_call_match(arg, fbt_bindings, matcher))
        }
        _ => false,
    }
}

fn body_has_call_match(
    body: &ast::FunctionBody<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    body.statements
        .iter()
        .any(|stmt| stmt_has_call_match(stmt, fbt_bindings, matcher))
}

fn arg_has_call_match(
    arg: &ast::Argument<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match arg {
        ast::Argument::SpreadElement(spread) => {
            expr_has_call_match(&spread.argument, fbt_bindings, matcher)
        }
        _ => expr_has_call_match(arg.to_expression(), fbt_bindings, matcher),
    }
}

fn expr_has_call_match(
    expr: &ast::Expression<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match expr {
        ast::Expression::CallExpression(call_expr) => {
            matcher(call_expr, fbt_bindings)
                || expr_has_call_match(&call_expr.callee, fbt_bindings, matcher)
                || call_expr
                    .arguments
                    .iter()
                    .any(|arg| arg_has_call_match(arg, fbt_bindings, matcher))
        }
        ast::Expression::JSXElement(jsx) => jsx_has_call_match(jsx, fbt_bindings, matcher),
        ast::Expression::JSXFragment(frag) => {
            jsx_fragment_has_call_match(frag, fbt_bindings, matcher)
        }
        ast::Expression::ConditionalExpression(cond) => {
            expr_has_call_match(&cond.test, fbt_bindings, matcher)
                || expr_has_call_match(&cond.consequent, fbt_bindings, matcher)
                || expr_has_call_match(&cond.alternate, fbt_bindings, matcher)
        }
        ast::Expression::LogicalExpression(logical) => {
            expr_has_call_match(&logical.left, fbt_bindings, matcher)
                || expr_has_call_match(&logical.right, fbt_bindings, matcher)
        }
        ast::Expression::AssignmentExpression(assign) => {
            expr_has_call_match(&assign.right, fbt_bindings, matcher)
        }
        ast::Expression::SequenceExpression(seq) => seq
            .expressions
            .iter()
            .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
        ast::Expression::ParenthesizedExpression(paren) => {
            expr_has_call_match(&paren.expression, fbt_bindings, matcher)
        }
        ast::Expression::TSAsExpression(ts) => {
            expr_has_call_match(&ts.expression, fbt_bindings, matcher)
        }
        ast::Expression::TSSatisfiesExpression(ts) => {
            expr_has_call_match(&ts.expression, fbt_bindings, matcher)
        }
        ast::Expression::TSNonNullExpression(ts) => {
            expr_has_call_match(&ts.expression, fbt_bindings, matcher)
        }
        ast::Expression::TSTypeAssertion(ts) => {
            expr_has_call_match(&ts.expression, fbt_bindings, matcher)
        }
        ast::Expression::AwaitExpression(await_expr) => {
            expr_has_call_match(&await_expr.argument, fbt_bindings, matcher)
        }
        ast::Expression::ObjectExpression(obj) => obj.properties.iter().any(|prop| match prop {
            ast::ObjectPropertyKind::ObjectProperty(p) => {
                expr_has_call_match(&p.value, fbt_bindings, matcher)
                    || p.key
                        .as_expression()
                        .is_some_and(|key| expr_has_call_match(key, fbt_bindings, matcher))
            }
            ast::ObjectPropertyKind::SpreadProperty(spread) => {
                expr_has_call_match(&spread.argument, fbt_bindings, matcher)
            }
        }),
        ast::Expression::ArrayExpression(arr) => arr.elements.iter().any(|elem| match elem {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                expr_has_call_match(&spread.argument, fbt_bindings, matcher)
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => expr_has_call_match(elem.to_expression(), fbt_bindings, matcher),
        }),
        ast::Expression::NewExpression(new_expr) => {
            expr_has_call_match(&new_expr.callee, fbt_bindings, matcher)
                || new_expr
                    .arguments
                    .iter()
                    .any(|arg| arg_has_call_match(arg, fbt_bindings, matcher))
        }
        ast::Expression::TaggedTemplateExpression(tagged) => {
            expr_has_call_match(&tagged.tag, fbt_bindings, matcher)
                || tagged
                    .quasi
                    .expressions
                    .iter()
                    .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher))
        }
        ast::Expression::TemplateLiteral(template) => template
            .expressions
            .iter()
            .any(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
        ast::Expression::BinaryExpression(bin) => {
            expr_has_call_match(&bin.left, fbt_bindings, matcher)
                || expr_has_call_match(&bin.right, fbt_bindings, matcher)
        }
        ast::Expression::UnaryExpression(unary) => {
            expr_has_call_match(&unary.argument, fbt_bindings, matcher)
        }
        ast::Expression::StaticMemberExpression(member) => {
            expr_has_call_match(&member.object, fbt_bindings, matcher)
        }
        ast::Expression::ComputedMemberExpression(member) => {
            expr_has_call_match(&member.object, fbt_bindings, matcher)
                || expr_has_call_match(&member.expression, fbt_bindings, matcher)
        }
        ast::Expression::YieldExpression(yield_expr) => yield_expr
            .argument
            .as_ref()
            .is_some_and(|arg| expr_has_call_match(arg, fbt_bindings, matcher)),
        _ => false,
    }
}

fn jsx_has_call_match(
    jsx: &ast::JSXElement<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    jsx.opening_element
        .attributes
        .iter()
        .any(|attr| jsx_attr_has_call_match(attr, fbt_bindings, matcher))
        || jsx
            .children
            .iter()
            .any(|child| jsx_child_has_call_match(child, fbt_bindings, matcher))
}

fn jsx_fragment_has_call_match(
    frag: &ast::JSXFragment<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    frag.children
        .iter()
        .any(|child| jsx_child_has_call_match(child, fbt_bindings, matcher))
}

fn jsx_attr_has_call_match(
    attr: &ast::JSXAttributeItem<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match attr {
        ast::JSXAttributeItem::Attribute(attr) => {
            attr.value.as_ref().is_some_and(|value| match value {
                ast::JSXAttributeValue::ExpressionContainer(container) => container
                    .expression
                    .as_expression()
                    .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
                ast::JSXAttributeValue::Element(elem) => {
                    jsx_has_call_match(elem, fbt_bindings, matcher)
                }
                ast::JSXAttributeValue::Fragment(frag) => {
                    jsx_fragment_has_call_match(frag, fbt_bindings, matcher)
                }
                _ => false,
            })
        }
        ast::JSXAttributeItem::SpreadAttribute(spread) => {
            expr_has_call_match(&spread.argument, fbt_bindings, matcher)
        }
    }
}

fn jsx_child_has_call_match(
    child: &ast::JSXChild<'_>,
    fbt_bindings: &NameSet,
    matcher: CallMatcher,
) -> bool {
    match child {
        ast::JSXChild::ExpressionContainer(container) => container
            .expression
            .as_expression()
            .is_some_and(|expr| expr_has_call_match(expr, fbt_bindings, matcher)),
        ast::JSXChild::Element(elem) => jsx_has_call_match(elem, fbt_bindings, matcher),
        ast::JSXChild::Fragment(frag) => jsx_fragment_has_call_match(frag, fbt_bindings, matcher),
        ast::JSXChild::Spread(spread) => {
            expr_has_call_match(&spread.expression, fbt_bindings, matcher)
        }
        _ => false,
    }
}

fn is_fbt_param_call_with_nested_fbt_value(
    call_expr: &ast::CallExpression<'_>,
    fbt_bindings: &NameSet,
) -> bool {
    if !is_fbt_param_callee(&call_expr.callee, fbt_bindings) {
        return false;
    }
    let Some(param_value) = call_expr.arguments.get(1) else {
        return false;
    };
    match param_value {
        ast::Argument::SpreadElement(spread) => {
            expr_contains_fbt_root_call(&spread.argument, fbt_bindings)
        }
        _ => expr_contains_fbt_root_call(param_value.to_expression(), fbt_bindings),
    }
}

fn expr_contains_fbt_root_call(expr: &ast::Expression<'_>, fbt_bindings: &NameSet) -> bool {
    expr_has_call_match(expr, fbt_bindings, is_fbt_root_call)
}

fn is_fbt_root_call(call_expr: &ast::CallExpression<'_>, fbt_bindings: &NameSet) -> bool {
    matches!(
        &call_expr.callee,
        ast::Expression::Identifier(id) if fbt_bindings.contains(id.name.as_str())
    )
}

fn is_fbt_param_callee(callee: &ast::Expression<'_>, fbt_bindings: &NameSet) -> bool {
    match callee {
        ast::Expression::StaticMemberExpression(member) => {
            member.property.name.as_str() == "param"
                && matches!(
                    &member.object,
                    ast::Expression::Identifier(id) if fbt_bindings.contains(id.name.as_str())
                )
        }
        ast::Expression::ComputedMemberExpression(member) => {
            matches!(
                &member.object,
                ast::Expression::Identifier(id) if fbt_bindings.contains(id.name.as_str())
            ) && matches!(
                &member.expression,
                ast::Expression::StringLiteral(lit) if lit.value.as_str() == "param"
            )
        }
        _ => false,
    }
}

fn collect_top_level_statement_bindings(
    stmt: &ast::Statement<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match stmt {
        ast::Statement::ImportDeclaration(import_decl) => {
            if import_decl.import_kind == ast::ImportOrExportKind::Type {
                return;
            }
            if let Some(specifiers) = &import_decl.specifiers {
                for specifier in specifiers {
                    match specifier {
                        ast::ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                            if spec.import_kind == ast::ImportOrExportKind::Value {
                                names.insert(spec.local.name.as_str().to_string());
                            }
                        }
                        ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                            names.insert(spec.local.name.as_str().to_string());
                        }
                        ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                            names.insert(spec.local.name.as_str().to_string());
                        }
                    }
                }
            }
        }
        ast::Statement::ExportNamedDeclaration(export_decl) => {
            if let Some(decl) = &export_decl.declaration {
                collect_top_level_declaration_bindings(decl, names);
            }
        }
        ast::Statement::ExportDefaultDeclaration(export_decl) => match &export_decl.declaration {
            ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    names.insert(id.name.as_str().to_string());
                }
            }
            ast::ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(id) = &class.id {
                    names.insert(id.name.as_str().to_string());
                }
            }
            _ => {}
        },
        ast::Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                collect_binding_pattern_names_owned(&declarator.id, names);
            }
        }
        ast::Statement::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Statement::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Statement::TSEnumDeclaration(decl) => {
            if !decl.declare {
                names.insert(decl.id.name.as_str().to_string());
            }
        }
        ast::Statement::TSModuleDeclaration(decl) => {
            if !decl.declare
                && let ast::TSModuleDeclarationName::Identifier(id) = &decl.id
            {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Statement::TSImportEqualsDeclaration(decl) => {
            if decl.import_kind == ast::ImportOrExportKind::Value {
                names.insert(decl.id.name.as_str().to_string());
            }
        }
        _ => {}
    }
}

fn collect_top_level_declaration_bindings(
    decl: &ast::Declaration<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match decl {
        ast::Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                collect_binding_pattern_names_owned(&declarator.id, names);
            }
        }
        ast::Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Declaration::TSEnumDeclaration(decl) => {
            if !decl.declare {
                names.insert(decl.id.name.as_str().to_string());
            }
        }
        ast::Declaration::TSModuleDeclaration(decl) => {
            if !decl.declare
                && let ast::TSModuleDeclarationName::Identifier(id) = &decl.id
            {
                names.insert(id.name.as_str().to_string());
            }
        }
        ast::Declaration::TSImportEqualsDeclaration(decl) => {
            if decl.import_kind == ast::ImportOrExportKind::Value {
                names.insert(decl.id.name.as_str().to_string());
            }
        }
        _ => {}
    }
}

fn collect_binding_pattern_names_owned(
    pattern: &ast::BindingPattern<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match pattern {
        ast::BindingPattern::BindingIdentifier(ident) => {
            names.insert(ident.name.as_str().to_string());
        }
        ast::BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_pattern_names_owned(&prop.value, names);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_pattern_names_owned(&rest.argument, names);
            }
        }
        ast::BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_binding_pattern_names_owned(elem, names);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_pattern_names_owned(&rest.argument, names);
            }
        }
        ast::BindingPattern::AssignmentPattern(assign) => {
            collect_binding_pattern_names_owned(&assign.left, names);
        }
    }
}

/// Generate a unique identifier name that doesn't collide with any existing names.
/// Follows Babel's `generateUid` pattern: tries `name`, then `name2`, `name3`, etc.
pub(crate) fn generate_unique_name(
    base: &str,
    existing: &std::collections::HashSet<String>,
) -> String {
    if !existing.contains(base) {
        return base.to_string();
    }
    let mut i = 2u32;
    loop {
        let candidate = format!("{}{}", base, i);
        if !existing.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// Generate a unique import binding name mirroring Babel's uid style:
/// `name` (if free) otherwise `_name`, `_name2`, `_name3`, ...
pub(crate) fn generate_unique_import_binding(
    base: &str,
    existing: &std::collections::HashSet<String>,
) -> String {
    if !existing.contains(base) {
        return base.to_string();
    }
    let prefixed = format!("_{}", base);
    if !existing.contains(&prefixed) {
        return prefixed;
    }
    let mut i = 2u32;
    loop {
        let candidate = format!("{}{}", prefixed, i);
        if !existing.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

/// Collect all binding names from the entire program (including inner function scopes).
/// This is used to detect naming conflicts for generated imports like `_c`.
pub(crate) fn collect_all_program_bindings(
    program: &ast::Program<'_>,
) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    for stmt in &program.body {
        collect_all_statement_bindings(stmt, &mut names);
    }
    names
}

fn collect_all_statement_bindings(
    stmt: &ast::Statement<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match stmt {
        ast::Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                collect_binding_pattern_names_owned(&declarator.id, names);
                if let Some(init) = &declarator.init {
                    collect_all_expression_bindings(init, names);
                }
            }
        }
        ast::Statement::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                names.insert(id.name.as_str().to_string());
            }
            collect_all_function_bindings(func, names);
        }
        ast::Statement::ImportDeclaration(import_decl) => {
            if let Some(specifiers) = &import_decl.specifiers {
                for specifier in specifiers {
                    match specifier {
                        ast::ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                            names.insert(spec.local.name.as_str().to_string());
                        }
                        ast::ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                            names.insert(spec.local.name.as_str().to_string());
                        }
                        ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                            names.insert(spec.local.name.as_str().to_string());
                        }
                    }
                }
            }
        }
        ast::Statement::ExportNamedDeclaration(export_decl) => {
            if let Some(decl) = &export_decl.declaration {
                collect_all_declaration_bindings(decl, names);
            }
        }
        ast::Statement::ExportDefaultDeclaration(export_decl) => {
            if let ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) =
                &export_decl.declaration
            {
                if let Some(id) = &func.id {
                    names.insert(id.name.as_str().to_string());
                }
                collect_all_function_bindings(func, names);
            }
        }
        ast::Statement::BlockStatement(block) => {
            for s in &block.body {
                collect_all_statement_bindings(s, names);
            }
        }
        ast::Statement::IfStatement(if_stmt) => {
            collect_all_statement_bindings(&if_stmt.consequent, names);
            if let Some(alt) = &if_stmt.alternate {
                collect_all_statement_bindings(alt, names);
            }
        }
        ast::Statement::ForStatement(for_stmt) => {
            if let Some(ast::ForStatementInit::VariableDeclaration(var_decl)) = &for_stmt.init {
                for declarator in &var_decl.declarations {
                    collect_binding_pattern_names_owned(&declarator.id, names);
                }
            }
            collect_all_statement_bindings(&for_stmt.body, names);
        }
        ast::Statement::ReturnStatement(_)
        | ast::Statement::ExpressionStatement(_)
        | ast::Statement::ThrowStatement(_)
        | ast::Statement::BreakStatement(_)
        | ast::Statement::ContinueStatement(_)
        | ast::Statement::EmptyStatement(_)
        | ast::Statement::DebuggerStatement(_) => {}
        _ => {
            // For other statement types (while, do-while, switch, try, etc.)
            // we don't need to recurse deeply for the purpose of detecting
            // `_c` conflicts. The most common cases are covered above.
        }
    }
}

fn collect_all_declaration_bindings(
    decl: &ast::Declaration<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match decl {
        ast::Declaration::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                collect_binding_pattern_names_owned(&declarator.id, names);
                if let Some(init) = &declarator.init {
                    collect_all_expression_bindings(init, names);
                }
            }
        }
        ast::Declaration::FunctionDeclaration(func) => {
            if let Some(id) = &func.id {
                names.insert(id.name.as_str().to_string());
            }
            collect_all_function_bindings(func, names);
        }
        _ => {}
    }
}

fn collect_all_function_bindings(
    func: &ast::Function<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    for param in &func.params.items {
        collect_binding_pattern_names_owned(&param.pattern, names);
    }
    if let Some(body) = &func.body {
        for stmt in &body.statements {
            collect_all_statement_bindings(stmt, names);
        }
    }
}

fn collect_all_expression_bindings(
    expr: &ast::Expression<'_>,
    names: &mut std::collections::HashSet<String>,
) {
    match expr {
        ast::Expression::ArrowFunctionExpression(arrow) => {
            for param in &arrow.params.items {
                collect_binding_pattern_names_owned(&param.pattern, names);
            }
            for stmt in &arrow.body.statements {
                collect_all_statement_bindings(stmt, names);
            }
        }
        ast::Expression::FunctionExpression(func) => {
            if let Some(id) = &func.id {
                names.insert(id.name.as_str().to_string());
            }
            collect_all_function_bindings(func, names);
        }
        _ => {}
    }
}

fn function_has_binding_named(func: &ast::Function<'_>, name: &str) -> bool {
    let mut names = std::collections::HashSet::new();
    collect_all_function_bindings(func, &mut names);
    names.contains(name)
}

fn arrow_has_binding_named(arrow: &ast::ArrowFunctionExpression<'_>, name: &str) -> bool {
    let mut names = std::collections::HashSet::new();
    for param in &arrow.params.items {
        collect_binding_pattern_names_owned(&param.pattern, &mut names);
    }
    for stmt in &arrow.body.statements {
        collect_all_statement_bindings(stmt, &mut names);
    }
    names.contains(name)
}

fn conflicting_global_bailout(name: &str) -> CompilerError {
    CompilerError::Bail(crate::error::BailOut {
        reason: "Encountered conflicting global in generated program".to_string(),
        diagnostics: vec![crate::error::CompilerDiagnostic {
            severity: crate::error::DiagnosticSeverity::Todo,
            message: format!("Conflict from local binding {}.", name),
        }],
    })
}

pub(crate) fn has_early_binding_reference(before: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    for (idx, _) in before.match_indices(ident) {
        let start_ok = before[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_identifier_char(c));
        let end = idx + ident.len();
        let end_ok = before[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_identifier_char(c));
        if !start_ok || !end_ok {
            continue;
        }

        let prev_non_ws = before[..idx].chars().rev().find(|c| !c.is_whitespace());
        let next_non_ws = before[end..].chars().find(|c| !c.is_whitespace());

        if matches!(prev_non_ws, Some('.') | Some(':'))
            || matches!(next_non_ws, Some(':'))
            || before[..idx].trim_end().ends_with(" as")
        {
            continue;
        }

        let prev_ok = matches!(prev_non_ws, Some('(') | Some(',') | Some('='));
        let next_ok = matches!(next_non_ws, Some(')') | Some(',') | Some(';') | Some('}'));
        if prev_ok && next_ok {
            return true;
        }
    }
    false
}

fn is_valid_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let first_ok = first == '_' || first == '$' || first.is_ascii_alphabetic();
    if !first_ok {
        return false;
    }
    let chars_ok = chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric());
    if !chars_ok {
        return false;
    }
    // Minimal reserved-word guard needed for dynamic gating directives.
    !matches!(
        name,
        "true"
            | "false"
            | "null"
            | "this"
            | "new"
            | "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "switch"
            | "case"
            | "default"
            | "function"
            | "class"
            | "import"
            | "export"
    )
}

fn collect_dynamic_gating_directives(source: &str, quote: char) -> Vec<String> {
    let pattern = format!("{quote}use memo if(");
    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(rel) = source[start..].find(&pattern) {
        let open = start + rel + pattern.len();
        if let Some(close_rel) = source[open..].find(')') {
            let close = open + close_rel;
            let ident = source[open..close].trim();
            if !ident.is_empty() {
                out.push(ident.to_string());
            }
            start = close + 1;
        } else {
            break;
        }
    }
    out
}

fn parse_dynamic_gating_identifier(source: &str) -> Option<String> {
    let mut directives = collect_dynamic_gating_directives(source, '"');
    directives.extend(collect_dynamic_gating_directives(source, '\''));
    if directives.len() != 1 {
        return None;
    }
    let ident = directives.pop()?;
    if is_valid_js_identifier(&ident) {
        Some(ident)
    } else {
        None
    }
}

/// Collect all compilable functions from a statement.
fn collect_compilable_functions<'a>(
    stmt: &ast::Statement<'a>,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
    compiled: &mut Vec<CompiledFunction>,
) {
    match stmt {
        ast::Statement::FunctionDeclaration(func) => {
            if std::env::var("DEBUG_LOWER").is_ok() {
                let fn_name = func
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str())
                    .unwrap_or("<anon>");
                eprintln!("[COLLECT] FunctionDeclaration: {}", fn_name);
            }
            match try_compile_function(
                func,
                true,
                source,
                semantic,
                options,
                custom_dirs,
                ignore_opt_out,
            ) {
                Ok(Some(cf)) => compiled.push(cf),
                Ok(None) => {}
                Err(e) => {
                    FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                    if std::env::var("DEBUG_LOWER").is_ok() {
                        let fn_name = func
                            .id
                            .as_ref()
                            .map(|id| id.name.as_str())
                            .unwrap_or("<anon>");
                        eprintln!("[BAIL] {}: {}", fn_name, e);
                    }
                }
            }
        }
        ast::Statement::ExportDefaultDeclaration(export) => {
            match &export.declaration {
                ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    match try_compile_function(
                        func,
                        true,
                        source,
                        semantic,
                        options,
                        custom_dirs,
                        ignore_opt_out,
                    ) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                let fn_name = func
                                    .id
                                    .as_ref()
                                    .map(|id| id.name.as_str())
                                    .unwrap_or("<anon>");
                                eprintln!("[BAIL] {}: {}", fn_name, e);
                            }
                        }
                    }
                }
                ast::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
                    // Default-exported anonymous arrows are typically components.
                    match try_compile_arrow(
                        "Component",
                        arrow,
                        source,
                        semantic,
                        options,
                        custom_dirs,
                        ignore_opt_out,
                    ) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                eprintln!("[BAIL] <export-default-arrow>: {}", e);
                            }
                        }
                    }
                }
                ast::ExportDefaultDeclarationKind::FunctionExpression(func) => {
                    if func.id.is_some() {
                        match try_compile_function(
                            func,
                            false,
                            source,
                            semantic,
                            options,
                            custom_dirs,
                            ignore_opt_out,
                        ) {
                            Ok(Some(cf)) => compiled.push(cf),
                            Ok(None) => {}
                            Err(e) => {
                                FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                                if std::env::var("DEBUG_LOWER").is_ok() {
                                    let fn_name = func
                                        .id
                                        .as_ref()
                                        .map(|id| id.name.as_str())
                                        .unwrap_or("<anon>");
                                    eprintln!("[BAIL] {}: {}", fn_name, e);
                                }
                            }
                        }
                    } else {
                        match try_compile_function_with_name(
                            func,
                            "Component",
                            source,
                            semantic,
                            options,
                        ) {
                            Ok(Some(cf)) => compiled.push(cf),
                            Ok(None) => {}
                            Err(e) => {
                                FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                                if std::env::var("DEBUG_LOWER").is_ok() {
                                    eprintln!("[BAIL] <export-default-fnexpr>: {}", e);
                                }
                            }
                        }
                    }
                }
                ast::ExportDefaultDeclarationKind::CallExpression(call) => {
                    // export default memo(() => ...) / forwardRef(() => ...)
                    if (is_react_api_callee(&call.callee, "forwardRef")
                        || is_react_api_callee(&call.callee, "memo"))
                        && let Some(first_arg) = call.arguments.first()
                        && !matches!(first_arg, ast::Argument::SpreadElement(_))
                    {
                        let arg_expr: &ast::Expression<'a> =
                            unsafe { std::mem::transmute(first_arg) };
                        collect_memo_wrapped_function(
                            arg_expr,
                            source,
                            semantic,
                            options,
                            custom_dirs,
                            ignore_opt_out,
                            compiled,
                        );
                    }
                }
                _ => {}
            }
        }
        ast::Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                match decl {
                    ast::Declaration::FunctionDeclaration(func) => {
                        match try_compile_function(
                            func,
                            true,
                            source,
                            semantic,
                            options,
                            custom_dirs,
                            ignore_opt_out,
                        ) {
                            Ok(Some(cf)) => compiled.push(cf),
                            Ok(None) => {}
                            Err(e) => {
                                FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                                if std::env::var("DEBUG_LOWER").is_ok() {
                                    let fn_name = func
                                        .id
                                        .as_ref()
                                        .map(|id| id.name.as_str())
                                        .unwrap_or("<anon>");
                                    eprintln!("[BAIL] {}: {}", fn_name, e);
                                }
                            }
                        }
                    }
                    ast::Declaration::VariableDeclaration(var_decl) => {
                        collect_var_decl_functions(
                            var_decl,
                            source,
                            semantic,
                            options,
                            custom_dirs,
                            ignore_opt_out,
                            compiled,
                        );
                    }
                    _ => {}
                }
            }
        }
        ast::Statement::VariableDeclaration(var_decl) => {
            collect_var_decl_functions(
                var_decl,
                source,
                semantic,
                options,
                custom_dirs,
                ignore_opt_out,
                compiled,
            );
        }
        ast::Statement::ExpressionStatement(expr_stmt) => {
            // Handle standalone React.memo(fn) / forwardRef(fn) expression statements
            collect_expression_statement_functions(
                &expr_stmt.expression,
                source,
                semantic,
                options,
                custom_dirs,
                ignore_opt_out,
                compiled,
            );
        }
        _ => {}
    }
}

/// Collect compilable arrow/function expressions from variable declarations.
fn collect_var_decl_functions<'a>(
    var_decl: &ast::VariableDeclaration<'a>,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
    compiled: &mut Vec<CompiledFunction>,
) {
    let mode = options.compilation_mode;
    for declarator in &var_decl.declarations {
        let name = match &declarator.id {
            ast::BindingPattern::BindingIdentifier(ident) => ident.name.as_str(),
            _ => continue,
        };
        let Some(init) = &declarator.init else {
            continue;
        };

        // Check for forwardRef/memo wrappers: const Foo = React.memo(() => ...)
        let wrapped_init = is_forwardref_or_memo_arg(init);
        let effective_init = wrapped_init.unwrap_or(init);
        let is_wrapper_function_arg = wrapped_init.is_some();

        match effective_init {
            ast::Expression::ArrowFunctionExpression(arrow) => {
                match try_compile_arrow(
                    name,
                    arrow,
                    source,
                    semantic,
                    options,
                    custom_dirs,
                    ignore_opt_out,
                ) {
                    Ok(Some(cf)) => compiled.push(cf),
                    Ok(None) => {}
                    Err(e) => {
                        FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                        if std::env::var("DEBUG_LOWER").is_ok() {
                            eprintln!("[BAIL] {}: {}", name, e);
                        }
                    }
                }
            }
            ast::Expression::FunctionExpression(func) => {
                if is_wrapper_function_arg {
                    // Parity with upstream: preserve anonymous callback form
                    // when compiling function expressions wrapped by memo/forwardRef.
                    match try_compile_function_with_name(func, "", source, semantic, options) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                eprintln!("[BAIL] <memo-wrapped-fn>: {}", e);
                            }
                        }
                    }
                    continue;
                }
                // For function expressions without an ID, use the variable name
                if func.id.is_some() {
                    match try_compile_function(
                        func,
                        false,
                        source,
                        semantic,
                        options,
                        custom_dirs,
                        ignore_opt_out,
                    ) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                let fn_name =
                                    func.id.as_ref().map(|id| id.name.as_str()).unwrap_or(name);
                                eprintln!("[BAIL] {}: {}", fn_name, e);
                            }
                        }
                    }
                } else {
                    // Anonymous function expression — use variable name for skip-logic
                    if let Some(body) = &func.body {
                        if !ignore_opt_out && has_function_opt_out(body, custom_dirs) {
                            // Skip compilation
                        } else if should_compile_function(name, body, &func.params, mode) {
                            match try_compile_function_with_name(
                                func, name, source, semantic, options,
                            ) {
                                Ok(Some(cf)) => compiled.push(cf),
                                Ok(None) => {}
                                Err(e) => {
                                    FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                                    if std::env::var("DEBUG_LOWER").is_ok() {
                                        eprintln!("[BAIL] {}: {}", name, e);
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

/// Handle standalone expression statements containing React.memo(fn) or forwardRef(fn).
/// These are bare `React.memo(props => ...)` calls not assigned to a variable.
/// In infer mode, functions inside memo/forwardRef are always treated as components.
fn collect_expression_statement_functions<'a>(
    expr: &ast::Expression<'a>,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
    compiled: &mut Vec<CompiledFunction>,
) {
    if let ast::Expression::CallExpression(call) = expr
        && (is_react_api_callee(&call.callee, "forwardRef")
            || is_react_api_callee(&call.callee, "memo"))
        && let Some(first_arg) = call.arguments.first()
        && !matches!(first_arg, ast::Argument::SpreadElement(_))
    {
        let arg_expr: &ast::Expression<'a> = unsafe { std::mem::transmute(first_arg) };
        // Functions inside memo/forwardRef are always compiled (forced component)
        collect_memo_wrapped_function(
            arg_expr,
            source,
            semantic,
            options,
            custom_dirs,
            ignore_opt_out,
            compiled,
        );
    } else if let ast::Expression::AssignmentExpression(assign) = expr {
        let name = match &assign.left {
            ast::AssignmentTarget::AssignmentTargetIdentifier(ident) => ident.name.as_str(),
            _ => return,
        };

        let effective_right = is_forwardref_or_memo_arg(&assign.right).unwrap_or(&assign.right);
        match effective_right {
            ast::Expression::ArrowFunctionExpression(arrow) => {
                match try_compile_arrow(
                    name,
                    arrow,
                    source,
                    semantic,
                    options,
                    custom_dirs,
                    ignore_opt_out,
                ) {
                    Ok(Some(cf)) => compiled.push(cf),
                    Ok(None) => {}
                    Err(e) => {
                        FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                        if std::env::var("DEBUG_LOWER").is_ok() {
                            eprintln!("[BAIL] {}: {}", name, e);
                        }
                    }
                }
            }
            ast::Expression::FunctionExpression(func) => {
                if func.id.is_some() {
                    match try_compile_function(
                        func,
                        false,
                        source,
                        semantic,
                        options,
                        custom_dirs,
                        ignore_opt_out,
                    ) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                let fn_name =
                                    func.id.as_ref().map(|id| id.name.as_str()).unwrap_or(name);
                                eprintln!("[BAIL] {}: {}", fn_name, e);
                            }
                        }
                    }
                } else {
                    if let Some(body) = &func.body
                        && !ignore_opt_out
                        && has_function_opt_out(body, custom_dirs)
                    {
                        return;
                    }
                    match try_compile_function_with_name(func, name, source, semantic, options) {
                        Ok(Some(cf)) => compiled.push(cf),
                        Ok(None) => {}
                        Err(e) => {
                            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                            if std::env::var("DEBUG_LOWER").is_ok() {
                                eprintln!("[BAIL] {}: {}", name, e);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Compile the inner function of a React.memo/forwardRef wrapper.
/// Always compiles (treats as component) regardless of compilation mode.
fn collect_memo_wrapped_function<'a>(
    expr: &ast::Expression<'a>,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
    compiled: &mut Vec<CompiledFunction>,
) {
    let mut options = options.clone();
    options.compilation_mode = CompilationMode::All;

    match expr {
        ast::Expression::ArrowFunctionExpression(arrow) => {
            // Force-compile: skip mode check, always compile
            if !ignore_opt_out && has_function_opt_out(&arrow.body, custom_dirs) {
                return;
            }
            // Use CompilationMode::All to force compilation regardless of name
            match try_compile_arrow(
                "",
                arrow,
                source,
                semantic,
                &options,
                custom_dirs,
                ignore_opt_out,
            ) {
                Ok(Some(cf)) => compiled.push(cf),
                Ok(None) => {}
                Err(e) => {
                    FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                    if std::env::var("DEBUG_LOWER").is_ok() {
                        eprintln!("[BAIL] <memo-arrow>: {}", e);
                    }
                }
            }
        }
        ast::Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body
                && !ignore_opt_out
                && has_function_opt_out(body, custom_dirs)
            {
                return;
            }
            if func.id.is_some() {
                match try_compile_function(
                    func,
                    false,
                    source,
                    semantic,
                    &options,
                    custom_dirs,
                    ignore_opt_out,
                ) {
                    Ok(Some(cf)) => compiled.push(cf),
                    Ok(None) => {}
                    Err(e) => {
                        FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                        if std::env::var("DEBUG_LOWER").is_ok() {
                            let fn_name = func
                                .id
                                .as_ref()
                                .map(|id| id.name.as_str())
                                .unwrap_or("<anon>");
                            eprintln!("[BAIL] {}: {}", fn_name, e);
                        }
                    }
                }
            } else {
                match try_compile_function_with_name(func, "", source, semantic, &options) {
                    Ok(Some(cf)) => compiled.push(cf),
                    Ok(None) => {}
                    Err(e) => {
                        FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
                        if std::env::var("DEBUG_LOWER").is_ok() {
                            eprintln!("[BAIL] <memo-fn>: {}", e);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn run_hir_pipeline_with_optional_retry(
    hir_func: HIRFunction,
    name: &str,
    options: &PluginOptions,
) -> Result<PipelineOutput, CompilerError> {
    RETRY_NO_MEMO_MODE.with(|flag| flag.set(false));
    let first_attempt = run_hir_pipeline(hir_func.clone(), name, &options.environment);
    match first_attempt {
        Ok(output) => Ok(output),
        Err(first_err) => {
            if options.panic_threshold != PanicThreshold::None {
                return Err(first_err);
            }

            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!(
                    "[PIPELINE_RETRY] {}: retrying without memoization after {:?}",
                    name, first_err
                );
            }

            RETRY_NO_MEMO_MODE.with(|flag| flag.set(true));
            let retry_result = run_hir_pipeline(hir_func, name, &options.environment);
            RETRY_NO_MEMO_MODE.with(|flag| flag.set(false));
            retry_result
        }
    }
}

fn should_skip_lowering_failure_in_retry_mode<'a>(
    body: &ast::FunctionBody<'a>,
    source: &str,
    options: &PluginOptions,
) -> bool {
    if options.panic_threshold != PanicThreshold::None {
        return false;
    }
    let retry_enabled =
        options.environment.enable_fire || options.environment.infer_effect_dependencies.is_some();
    if !retry_enabled {
        return false;
    }
    !body_source_contains_fire_call(body, source)
}

fn body_source_contains_fire_call<'a>(body: &ast::FunctionBody<'a>, source: &str) -> bool {
    let start = body.span.start as usize;
    let end = body.span.end as usize;
    let Some(slice) = source.get(start..end) else {
        return false;
    };
    source_text_contains_fire_call(slice)
}

fn source_text_contains_fire_call(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"fire" {
            let prev_ok = i == 0
                || !matches!(
                    bytes[i - 1] as char,
                    'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '$'
                );
            if prev_ok {
                let mut j = i + 4;
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Try to compile a function declaration. Returns `Ok(None)` if the function
/// should be intentionally skipped (opt-out / not compilable), `Err` on failure.
fn try_compile_function<'a>(
    func: &ast::Function<'a>,
    is_function_declaration: bool,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
) -> Result<Option<CompiledFunction>, CompilerError> {
    let mode = options.compilation_mode;
    let Some(name) = func.id.as_ref().map(|id| id.name.as_str()) else {
        return Ok(None);
    };

    let Some(body) = func.body.as_ref() else {
        return Ok(None);
    };

    // Check function-level opt-out (unless @ignoreUseNoForget)
    if !ignore_opt_out && has_function_opt_out(body, custom_dirs) {
        if std::env::var("DEBUG_LOWER").is_ok() {
            eprintln!("[SKIP_OPT_OUT] {}", name);
        }
        return Ok(None);
    }

    // Check if this function should be compiled based on compilation mode
    if !should_compile_function(name, body, &func.params, mode) {
        if std::env::var("DEBUG_LOWER").is_ok() {
            eprintln!("[SKIP_COMPILE] {}", name);
        }
        return Ok(None);
    }
    if options.environment.enable_emit_freeze && function_has_binding_named(func, "__DEV__") {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[BAILOUT_REASON] fn={} validator=emitFreeze reason=Conflict from local binding __DEV__.",
                name
            );
        }
        return Err(conflicting_global_bailout("__DEV__"));
    }

    // TODO: implement validate_no_dynamic_components_or_hooks
    let env = crate::environment::Environment::new(options.environment.clone());

    // Phase 1: Lower to HIR
    let lowering_cx = build::LoweringContext::new(semantic, source, env);
    let lower_result = match build::lower_function(
        body,
        &func.params,
        lowering_cx,
        build::LowerFunctionOptions::function(Some(name), func.span, func.generator, func.r#async),
    ) {
        Ok(r) => r,
        Err(e) => {
            if should_skip_lowering_failure_in_retry_mode(body, source, options) {
                if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                    eprintln!("[LOWER_SKIP_RETRY] {}: {:?}", name, e);
                }
                return Ok(None);
            }
            if std::env::var("DEBUG_LOWER").is_ok() {
                eprintln!("[LOWER_FAIL] {}: {:?}", name, e);
            }
            return Err(CompilerError::LoweringFailed(format!("{e:?}")));
        }
    };

    let mut hir_func = lower_result.func;
    if std::env::var("DEBUG_LOWER").is_ok() {
        eprintln!("[LOWERED] {} — {} blocks", name, hir_func.body.blocks.len());
    }

    // Set fn_type based on upstream-equivalent function classification.
    // Functions that aren't classified as Component/Hook remain Other.
    match get_react_function_type(name, body, &func.params) {
        Some("Component") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Component,
        Some("Hook") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Hook,
        _ => {}
    }

    // Compute parameter destructuring (reactive codegen inlines these in body).
    let mut temp_counter = 0;
    let mut params_result =
        params_to_result(&func.params, source, semantic, options, &mut temp_counter);

    // Run the shared HIR pipeline (all passes from pruneMaybeThrows through codegen)
    let pipeline_output = match run_hir_pipeline_with_optional_retry(hir_func, name, options) {
        Ok(output) => output,
        Err(e) => {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!("[PIPELINE_FAIL] {}: {:?}", name, e);
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None); // Validation error → skip this function
        }
    };

    // Upstream panicThreshold:"none" retry behavior can keep source unchanged for
    // no-memo retry fallbacks that only surfaced validation diagnostics.
    if pipeline_output.retry_no_memo_mode
        && !pipeline_output.has_fire_rewrite
        && !pipeline_output.has_inferred_effect
    {
        if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
            eprintln!(
                "[RETRY_SKIP_EMIT] {}: preserving source on no-memo retry",
                name
            );
        }
        return Ok(None);
    }

    let codegen_result = pipeline_output.codegen_result;
    align_params_result_with_codegen(&mut params_result, &codegen_result.param_names);
    let PreparedGeneratedBody {
        synthesized_default_param_cache,
        synthesized_hir_outlined_functions,
        cache_prologue,
    } = prepare_generated_body(&codegen_result, &params_result.prefix_statements);
    let normalize_use_fire_binding_temps =
        pipeline_output.retry_no_memo_mode && pipeline_output.has_fire_rewrite;

    let ParamsResult {
        compiled_params,
        prefix_statements,
        hir_outlined_functions: param_hir_outlined_functions,
    } = params_result;

    let pipeline_hir_outlined_functions: Vec<(String, HIRFunction)> = pipeline_output
        .hir_outlined
        .iter()
        .map(|of| {
            let mut hir_function = of.func.clone();
            hir_function.id = Some(of.name.clone());
            (of.name.clone(), hir_function)
        })
        .collect();
    let mut outlined = Vec::new();
    for of in &pipeline_output.hir_outlined {
        let Some(outlined_function) = codegen_outlined_function(
            &of.func,
            options.environment.enable_change_variable_codegen,
            &pipeline_output.reserved_removed_names,
        ) else {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!(
                    "[PIPELINE_FAIL] {}: outlined {} emitted no rendered body",
                    name, of.name
                );
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None);
        };
        if outlined_function_needs_backend_render(&outlined_function, &of.func) {
            outlined.push(outlined_function);
        }
    }
    dedupe_outlined_functions(&mut outlined);
    let mut hir_outlined_functions = param_hir_outlined_functions;
    hir_outlined_functions.extend(synthesized_hir_outlined_functions);
    hir_outlined_functions.extend(pipeline_hir_outlined_functions.clone());
    dedupe_hir_outlined_functions(&mut hir_outlined_functions);
    let needs_instrument_forget = options.environment.enable_emit_instrument_forget
        && codegen_result.needs_cache_import
        && !name.is_empty();
    let needs_emit_freeze =
        options.environment.enable_emit_freeze && codegen_result.needs_cache_import;
    let param_prefix_statements = if synthesized_default_param_cache.is_some() {
        vec![]
    } else if prefix_statements.iter().any(|statement| {
        matches!(
            &statement.init,
            CompiledInitializer::UndefinedFallback { .. }
        )
    }) {
        prefix_statements.clone()
    } else {
        vec![]
    };

    let needs_cache_import =
        codegen_result.needs_cache_import || synthesized_default_param_cache.is_some();

    Ok(Some(CompiledFunction {
        name: name.to_string(),
        start: func.span.start,
        end: func.span.end,
        reactive_function: Some(pipeline_output.reactive_function),
        needs_cache_import,
        compiled_params,
        param_prefix_statements,
        synthesized_default_param_cache,
        is_function_declaration,
        directives: extract_emitted_directives(body),
        hir_function: Some(pipeline_output.final_hir_snapshot),
        cache_prologue,
        needs_function_hook_guard_wrapper: codegen_result.needs_function_hook_guard_wrapper,
        normalize_use_fire_binding_temps,
        needs_instrument_forget,
        needs_emit_freeze,
        outlined_functions: outlined,
        hir_outlined_functions,
        has_fire_rewrite: pipeline_output.has_fire_rewrite,
        needs_hook_guards: codegen_result.needs_hook_guards,
        needs_structural_check_import: codegen_result.needs_structural_check_import,
        needs_lower_context_access: pipeline_output.has_lower_context_access,
        enable_change_variable_codegen: options.environment.enable_change_variable_codegen,
        enable_emit_hook_guards: options.environment.enable_emit_hook_guards,
        enable_change_detection_for_debugging: options
            .environment
            .enable_change_detection_for_debugging,
        enable_reset_cache_on_source_file_changes: options
            .environment
            .enable_reset_cache_on_source_file_changes
            .unwrap_or(false),
        fast_refresh_source_hash: get_fast_refresh_source_hash(),
        disable_memoization_features: pipeline_output.retry_no_memo_mode,
        disable_memoization_for_debugging: options.environment.disable_memoization_for_debugging,
        fbt_operands: pipeline_output.fbt_operands,
        unique_identifiers: pipeline_output.unique_identifiers,
        enable_name_anonymous_functions: options.environment.enable_name_anonymous_functions,
    }))
}

/// Try to compile an anonymous function expression, using the variable name.
/// Returns `Ok(None)` if the function has no body, `Err` on lowering failure.
fn try_compile_function_with_name<'a>(
    func: &ast::Function<'a>,
    name: &str,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
) -> Result<Option<CompiledFunction>, CompilerError> {
    let Some(body) = func.body.as_ref() else {
        return Ok(None);
    };
    if options.environment.enable_emit_freeze && function_has_binding_named(func, "__DEV__") {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[BAILOUT_REASON] fn={} validator=emitFreeze reason=Conflict from local binding __DEV__.",
                name
            );
        }
        return Err(conflicting_global_bailout("__DEV__"));
    }

    // TODO: implement validate_no_dynamic_components_or_hooks
    let env = crate::environment::Environment::new(options.environment.clone());

    // Phase 1: Lower to HIR
    let lowering_cx = build::LoweringContext::new(semantic, source, env);
    let lower_result = match build::lower_function(
        body,
        &func.params,
        lowering_cx,
        build::LowerFunctionOptions::function(Some(name), func.span, func.generator, func.r#async),
    ) {
        Ok(r) => r,
        Err(e) => {
            if should_skip_lowering_failure_in_retry_mode(body, source, options) {
                if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                    eprintln!("[LOWER_SKIP_RETRY] {}: {:?}", name, e);
                }
                return Ok(None);
            }
            return Err(CompilerError::LoweringFailed(format!("{e:?}")));
        }
    };

    let mut hir_func = lower_result.func;

    // Set fn_type based on function classification.
    match get_react_function_type(name, body, &func.params) {
        Some("Component") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Component,
        Some("Hook") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Hook,
        _ => {}
    }

    // Compute parameter destructuring (reactive codegen inlines these in body).
    let mut temp_counter = 0;
    let mut params_result =
        params_to_result(&func.params, source, semantic, options, &mut temp_counter);

    // Run the shared HIR pipeline
    let pipeline_output = match run_hir_pipeline_with_optional_retry(hir_func, name, options) {
        Ok(output) => output,
        Err(e) => {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!("[PIPELINE_FAIL] {}: {:?}", name, e);
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None); // Validation error → skip this function
        }
    };

    // Upstream panicThreshold:"none" retry behavior can keep source unchanged for
    // no-memo retry fallbacks that only surfaced validation diagnostics.
    if pipeline_output.retry_no_memo_mode
        && !pipeline_output.has_fire_rewrite
        && !pipeline_output.has_inferred_effect
    {
        if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
            eprintln!(
                "[RETRY_SKIP_EMIT] {}: preserving source on no-memo retry",
                name
            );
        }
        return Ok(None);
    }

    let codegen_result = pipeline_output.codegen_result;
    align_params_result_with_codegen(&mut params_result, &codegen_result.param_names);
    let PreparedGeneratedBody {
        synthesized_default_param_cache,
        synthesized_hir_outlined_functions,
        cache_prologue,
    } = prepare_generated_body(&codegen_result, &params_result.prefix_statements);
    let normalize_use_fire_binding_temps =
        pipeline_output.retry_no_memo_mode && pipeline_output.has_fire_rewrite;

    let ParamsResult {
        compiled_params,
        prefix_statements,
        hir_outlined_functions: param_hir_outlined_functions,
    } = params_result;

    let pipeline_hir_outlined_functions: Vec<(String, HIRFunction)> = pipeline_output
        .hir_outlined
        .iter()
        .map(|of| {
            let mut hir_function = of.func.clone();
            hir_function.id = Some(of.name.clone());
            (of.name.clone(), hir_function)
        })
        .collect();
    let mut outlined = Vec::new();
    for of in &pipeline_output.hir_outlined {
        let Some(outlined_function) = codegen_outlined_function(
            &of.func,
            options.environment.enable_change_variable_codegen,
            &pipeline_output.reserved_removed_names,
        ) else {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!(
                    "[PIPELINE_FAIL] {}: outlined {} emitted no rendered body",
                    name, of.name
                );
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None);
        };
        if outlined_function_needs_backend_render(&outlined_function, &of.func) {
            outlined.push(outlined_function);
        }
    }
    dedupe_outlined_functions(&mut outlined);
    let mut hir_outlined_functions = param_hir_outlined_functions;
    hir_outlined_functions.extend(synthesized_hir_outlined_functions);
    hir_outlined_functions.extend(pipeline_hir_outlined_functions.clone());
    dedupe_hir_outlined_functions(&mut hir_outlined_functions);
    let needs_instrument_forget = options.environment.enable_emit_instrument_forget
        && codegen_result.needs_cache_import
        && !name.is_empty();
    let needs_emit_freeze =
        options.environment.enable_emit_freeze && codegen_result.needs_cache_import;
    let param_prefix_statements = if synthesized_default_param_cache.is_some() {
        vec![]
    } else if prefix_statements.iter().any(|statement| {
        matches!(
            &statement.init,
            CompiledInitializer::UndefinedFallback { .. }
        )
    }) {
        prefix_statements.clone()
    } else {
        vec![]
    };

    let needs_cache_import =
        codegen_result.needs_cache_import || synthesized_default_param_cache.is_some();

    Ok(Some(CompiledFunction {
        name: name.to_string(),
        start: func.span.start,
        end: func.span.end,
        reactive_function: Some(pipeline_output.reactive_function),
        needs_cache_import,
        compiled_params,
        param_prefix_statements,
        synthesized_default_param_cache,
        is_function_declaration: false,
        directives: extract_emitted_directives(body),
        hir_function: Some(pipeline_output.final_hir_snapshot),
        cache_prologue,
        needs_function_hook_guard_wrapper: codegen_result.needs_function_hook_guard_wrapper,
        normalize_use_fire_binding_temps,
        needs_instrument_forget,
        needs_emit_freeze,
        outlined_functions: outlined,
        hir_outlined_functions,
        has_fire_rewrite: pipeline_output.has_fire_rewrite,
        needs_hook_guards: codegen_result.needs_hook_guards,
        needs_structural_check_import: codegen_result.needs_structural_check_import,
        needs_lower_context_access: pipeline_output.has_lower_context_access,
        enable_change_variable_codegen: options.environment.enable_change_variable_codegen,
        enable_emit_hook_guards: options.environment.enable_emit_hook_guards,
        enable_change_detection_for_debugging: options
            .environment
            .enable_change_detection_for_debugging,
        enable_reset_cache_on_source_file_changes: options
            .environment
            .enable_reset_cache_on_source_file_changes
            .unwrap_or(false),
        fast_refresh_source_hash: get_fast_refresh_source_hash(),
        disable_memoization_features: pipeline_output.retry_no_memo_mode,
        disable_memoization_for_debugging: options.environment.disable_memoization_for_debugging,
        fbt_operands: pipeline_output.fbt_operands,
        unique_identifiers: pipeline_output.unique_identifiers,
        enable_name_anonymous_functions: options.environment.enable_name_anonymous_functions,
    }))
}

/// Try to compile an arrow function expression assigned to a variable.
/// Returns `Ok(None)` for intentional skips, `Err` on lowering failure.
fn try_compile_arrow<'a>(
    name: &str,
    arrow: &ast::ArrowFunctionExpression<'a>,
    source: &str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    custom_dirs: &[String],
    ignore_opt_out: bool,
) -> Result<Option<CompiledFunction>, CompilerError> {
    let mode = options.compilation_mode;
    // Check function-level opt-out
    if !ignore_opt_out && has_function_opt_out(&arrow.body, custom_dirs) {
        return Ok(None);
    }

    // Check if this function should be compiled based on compilation mode
    if !should_compile_function(name, &arrow.body, &arrow.params, mode) {
        return Ok(None);
    }
    if options.environment.enable_emit_freeze && arrow_has_binding_named(arrow, "__DEV__") {
        if std::env::var("DEBUG_BAILOUT_REASON").is_ok() {
            eprintln!(
                "[BAILOUT_REASON] fn={} validator=emitFreeze reason=Conflict from local binding __DEV__.",
                name
            );
        }
        return Err(conflicting_global_bailout("__DEV__"));
    }

    // TODO: implement validate_no_dynamic_components_or_hooks
    let env = crate::environment::Environment::new(options.environment.clone());

    let lowering_cx = build::LoweringContext::new(semantic, source, env);
    let lower_result = match build::lower_arrow_expression(
        &arrow.body,
        &arrow.params,
        lowering_cx,
        build::LowerFunctionOptions::arrow(Some(name), arrow.span, arrow.r#async, arrow.expression),
    ) {
        Ok(r) => r,
        Err(e) => {
            if should_skip_lowering_failure_in_retry_mode(&arrow.body, source, options) {
                if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                    eprintln!("[LOWER_SKIP_RETRY] {}: {:?}", name, e);
                }
                return Ok(None);
            }
            return Err(CompilerError::LoweringFailed(format!("{e:?}")));
        }
    };

    let mut hir_func = lower_result.func;

    // Set fn_type based on function classification.
    match get_react_function_type(name, &arrow.body, &arrow.params) {
        Some("Component") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Component,
        Some("Hook") => hir_func.fn_type = crate::hir::types::ReactFunctionType::Hook,
        _ => {}
    }

    // Compute parameter destructuring (reactive codegen inlines these in body).
    let mut temp_counter = 0;
    let mut params_result =
        params_to_result(&arrow.params, source, semantic, options, &mut temp_counter);

    // Run the shared HIR pipeline
    let pipeline_output = match run_hir_pipeline_with_optional_retry(hir_func, name, options) {
        Ok(output) => output,
        Err(e) => {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!("[PIPELINE_FAIL] {}: {:?}", name, e);
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None); // Validation error → skip this function
        }
    };

    // Upstream panicThreshold:"none" retry behavior can keep source unchanged for
    // no-memo retry fallbacks that only surfaced validation diagnostics.
    if pipeline_output.retry_no_memo_mode
        && !pipeline_output.has_fire_rewrite
        && !pipeline_output.has_inferred_effect
    {
        if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
            eprintln!(
                "[RETRY_SKIP_EMIT] {}: preserving source on no-memo retry",
                name
            );
        }
        return Ok(None);
    }

    let codegen_result = pipeline_output.codegen_result;
    align_params_result_with_codegen(&mut params_result, &codegen_result.param_names);
    let PreparedGeneratedBody {
        synthesized_default_param_cache,
        synthesized_hir_outlined_functions,
        cache_prologue,
    } = prepare_generated_body(&codegen_result, &params_result.prefix_statements);
    let normalize_use_fire_binding_temps =
        pipeline_output.retry_no_memo_mode && pipeline_output.has_fire_rewrite;

    let ParamsResult {
        compiled_params,
        prefix_statements,
        hir_outlined_functions: param_hir_outlined_functions,
    } = params_result;

    let pipeline_hir_outlined_functions: Vec<(String, HIRFunction)> = pipeline_output
        .hir_outlined
        .iter()
        .map(|of| {
            let mut hir_function = of.func.clone();
            hir_function.id = Some(of.name.clone());
            (of.name.clone(), hir_function)
        })
        .collect();
    let mut outlined = Vec::new();
    for of in &pipeline_output.hir_outlined {
        let Some(outlined_function) = codegen_outlined_function(
            &of.func,
            options.environment.enable_change_variable_codegen,
            &pipeline_output.reserved_removed_names,
        ) else {
            if std::env::var("DEBUG_PIPELINE_ERRORS").is_ok() {
                eprintln!(
                    "[PIPELINE_FAIL] {}: outlined {} emitted no rendered body",
                    name, of.name
                );
            }
            FILE_HAD_PIPELINE_ERROR.with(|flag| flag.set(true));
            return Ok(None);
        };
        if outlined_function_needs_backend_render(&outlined_function, &of.func) {
            outlined.push(outlined_function);
        }
    }
    dedupe_outlined_functions(&mut outlined);
    let mut hir_outlined_functions = param_hir_outlined_functions;
    hir_outlined_functions.extend(synthesized_hir_outlined_functions);
    hir_outlined_functions.extend(pipeline_hir_outlined_functions.clone());
    dedupe_hir_outlined_functions(&mut hir_outlined_functions);
    let needs_instrument_forget = options.environment.enable_emit_instrument_forget
        && codegen_result.needs_cache_import
        && !name.is_empty();
    let needs_emit_freeze =
        options.environment.enable_emit_freeze && codegen_result.needs_cache_import;
    let param_prefix_statements = if synthesized_default_param_cache.is_some() {
        vec![]
    } else if prefix_statements.iter().any(|statement| {
        matches!(
            &statement.init,
            CompiledInitializer::UndefinedFallback { .. }
        )
    }) {
        prefix_statements.clone()
    } else {
        vec![]
    };

    let needs_cache_import =
        codegen_result.needs_cache_import || synthesized_default_param_cache.is_some();

    Ok(Some(CompiledFunction {
        name: name.to_string(),
        start: arrow.span.start,
        end: arrow.span.end,
        reactive_function: Some(pipeline_output.reactive_function),
        needs_cache_import,
        compiled_params,
        param_prefix_statements,
        synthesized_default_param_cache,
        is_function_declaration: false,
        directives: extract_emitted_directives(&arrow.body),
        hir_function: Some(pipeline_output.final_hir_snapshot),
        cache_prologue,
        needs_function_hook_guard_wrapper: codegen_result.needs_function_hook_guard_wrapper,
        normalize_use_fire_binding_temps,
        needs_instrument_forget,
        needs_emit_freeze,
        outlined_functions: outlined,
        hir_outlined_functions,
        has_fire_rewrite: pipeline_output.has_fire_rewrite,
        needs_hook_guards: codegen_result.needs_hook_guards,
        needs_structural_check_import: codegen_result.needs_structural_check_import,
        needs_lower_context_access: pipeline_output.has_lower_context_access,
        enable_change_variable_codegen: options.environment.enable_change_variable_codegen,
        enable_emit_hook_guards: options.environment.enable_emit_hook_guards,
        enable_change_detection_for_debugging: options
            .environment
            .enable_change_detection_for_debugging,
        enable_reset_cache_on_source_file_changes: options
            .environment
            .enable_reset_cache_on_source_file_changes
            .unwrap_or(false),
        fast_refresh_source_hash: get_fast_refresh_source_hash(),
        disable_memoization_features: pipeline_output.retry_no_memo_mode,
        disable_memoization_for_debugging: options.environment.disable_memoization_for_debugging,
        fbt_operands: pipeline_output.fbt_operands,
        unique_identifiers: pipeline_output.unique_identifiers,
        enable_name_anonymous_functions: options.environment.enable_name_anonymous_functions,
    }))
}

/// Result of parameter string generation.
struct ParamsResult {
    /// Structured rewritten params when they are plain identifiers/rest identifiers.
    compiled_params: Option<Vec<CompiledParam>>,
    /// Structured prefix statements to emit at the top of the function body.
    prefix_statements: Vec<CompiledParamPrefixStatement>,
    /// HIR-lowered outlined functions from source default parameter values.
    hir_outlined_functions: Vec<(String, HIRFunction)>,
}

fn is_identifier_token_char(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn replace_identifier_tokens_for_params(input: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from == to {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < input.len() {
        let rest = &input[i..];
        if let Some(found) = rest.find(from) {
            let start = i + found;
            let end = start + from.len();
            let before_ok = if start == 0 {
                true
            } else {
                !is_identifier_token_char(input.as_bytes()[start - 1] as char)
            };
            let after_ok = if end >= input.len() {
                true
            } else {
                !is_identifier_token_char(input.as_bytes()[end] as char)
            };
            out.push_str(&input[i..start]);
            if before_ok && after_ok {
                out.push_str(to);
            } else {
                out.push_str(from);
            }
            i = end;
        } else {
            out.push_str(rest);
            break;
        }
    }
    out
}

fn rename_compiled_binding_pattern(pattern: &mut CompiledBindingPattern, from: &str, to: &str) {
    match pattern {
        CompiledBindingPattern::Identifier(name) => {
            if name == from {
                *name = to.to_string();
            }
        }
        CompiledBindingPattern::Object(object) => {
            for property in &mut object.properties {
                if let CompiledPropertyKey::Source(source) = &mut property.key {
                    *source = replace_identifier_tokens_for_params(source, from, to);
                }
                rename_compiled_binding_pattern(&mut property.value, from, to);
            }
            if let Some(rest) = &mut object.rest {
                rename_compiled_binding_pattern(rest, from, to);
            }
        }
        CompiledBindingPattern::Array(array) => {
            for element in array.elements.iter_mut().flatten() {
                rename_compiled_binding_pattern(element, from, to);
            }
            if let Some(rest) = &mut array.rest {
                rename_compiled_binding_pattern(rest, from, to);
            }
        }
        CompiledBindingPattern::Assignment { left, default_expr } => {
            rename_compiled_binding_pattern(left, from, to);
            *default_expr = replace_identifier_tokens_for_params(default_expr, from, to);
        }
    }
}

fn rename_compiled_initializer(init: &mut CompiledInitializer, from: &str, to: &str) {
    match init {
        CompiledInitializer::Identifier(name) => {
            if name == from {
                *name = to.to_string();
            }
        }
        CompiledInitializer::UndefinedFallback {
            temp_name,
            default_expr,
        } => {
            if temp_name == from {
                *temp_name = to.to_string();
            }
            *default_expr = replace_identifier_tokens_for_params(default_expr, from, to);
        }
    }
}

fn rename_compiled_param_prefix_statement(
    statement: &mut CompiledParamPrefixStatement,
    from: &str,
    to: &str,
) {
    rename_compiled_binding_pattern(&mut statement.pattern, from, to);
    rename_compiled_initializer(&mut statement.init, from, to);
}

fn align_params_result_with_codegen(params_result: &mut ParamsResult, param_names: &[String]) {
    let Some(compiled_params) = params_result.compiled_params.as_mut() else {
        return;
    };
    if compiled_params.len() != param_names.len() {
        return;
    }

    // The emitted parameter list must follow the final HIR/codegen names, which
    // can differ from the source AST for lowered rest/default/destructured params.
    for (compiled_param, emitted_name) in compiled_params.iter_mut().zip(param_names) {
        if compiled_param.name == *emitted_name {
            continue;
        }
        let original_name = compiled_param.name.clone();
        for statement in &mut params_result.prefix_statements {
            rename_compiled_param_prefix_statement(statement, &original_name, emitted_name);
        }
        compiled_param.name = emitted_name.clone();
    }
}

struct PreparedGeneratedBody {
    synthesized_default_param_cache: Option<SynthesizedDefaultParamCache>,
    synthesized_hir_outlined_functions: Vec<(String, HIRFunction)>,
    cache_prologue: Option<crate::codegen_backend::codegen_ast::CachePrologue>,
}

fn prepare_generated_body(
    codegen_result: &crate::codegen_backend::codegen_ast::CodegenMetadata,
    _prefix_statements: &[CompiledParamPrefixStatement],
) -> PreparedGeneratedBody {
    PreparedGeneratedBody {
        synthesized_default_param_cache: None,
        synthesized_hir_outlined_functions: vec![],
        cache_prologue: codegen_result.cache_prologue.clone(),
    }
}

/// Generate a comma-separated parameter string from the AST, stripping type annotations.
/// Destructured parameters are replaced with temporaries (t0, t1, ...) and their
/// destructuring is moved to the function body, matching upstream behavior.
fn params_to_result<'a>(
    params: &ast::FormalParameters<'a>,
    source: &'a str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
    temp_counter: &mut usize,
) -> ParamsResult {
    let mut compiled_params: Option<Vec<CompiledParam>> = Some(Vec::new());
    let mut prefix_statements: Vec<CompiledParamPrefixStatement> = Vec::new();
    let mut hir_outlined_functions: Vec<(String, HIRFunction)> = Vec::new();
    let mut outline_counter: usize = 0;

    for param in &params.items {
        // Check for default value via FormalParameter::initializer (OXC stores `x = default` here)
        if let Some(initializer) = &param.initializer {
            let temp_name = format!("t{}", *temp_counter);
            *temp_counter += 1;
            if let Some(params) = compiled_params.as_mut() {
                params.push(CompiledParam {
                    name: temp_name.clone(),
                    is_rest: false,
                });
            }

            let default_start = initializer.span().start as usize;
            let default_end = initializer.span().end as usize;
            let default_expr = &source[default_start..default_end];

            // Check if default value is a function expression that can be outlined.
            // Upstream outlines anonymous function expressions with no captured context.
            let outlined_name = match initializer.without_parentheses() {
                ast::Expression::ArrowFunctionExpression(arrow) => {
                    if can_outline_arrow_default(arrow, source) {
                        outline_counter += 1;
                        let name = if outline_counter == 1 {
                            "_temp".to_string()
                        } else {
                            format!("_temp{}", outline_counter)
                        };
                        if let Some(hir_function) = try_lower_default_outlined_arrow(
                            &name, arrow, source, semantic, options,
                        ) {
                            hir_outlined_functions.push((name.clone(), hir_function));
                            Some(name)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                ast::Expression::FunctionExpression(func) if func.id.is_none() => {
                    if can_outline_func_default(func, source) {
                        outline_counter += 1;
                        let name = if outline_counter == 1 {
                            "_temp".to_string()
                        } else {
                            format!("_temp{}", outline_counter)
                        };
                        if let Some(hir_function) = try_lower_default_outlined_function(
                            &name, func, source, semantic, options,
                        ) {
                            hir_outlined_functions.push((name.clone(), hir_function));
                            Some(name)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => None,
            };

            // Use outlined name or raw default expression in the destructuring
            let effective_default = outlined_name.as_deref().unwrap_or(default_expr);

            match &param.pattern {
                ast::BindingPattern::BindingIdentifier(ident) => {
                    prefix_statements.push(compiled_prefix_statement(
                        ast::VariableDeclarationKind::Const,
                        CompiledBindingPattern::Identifier(ident.name.to_string()),
                        CompiledInitializer::UndefinedFallback {
                            temp_name: temp_name.clone(),
                            default_expr: effective_default.to_string(),
                        },
                    ));
                }
                ast::BindingPattern::ObjectPattern(_)
                | ast::BindingPattern::ArrayPattern(_)
                | ast::BindingPattern::AssignmentPattern(_) => {
                    prefix_statements.push(compiled_prefix_statement(
                        ast::VariableDeclarationKind::Const,
                        compiled_binding_pattern_from_ast(&param.pattern, source),
                        CompiledInitializer::UndefinedFallback {
                            temp_name: temp_name.clone(),
                            default_expr: effective_default.to_string(),
                        },
                    ))
                }
            }
            continue;
        }

        match &param.pattern {
            ast::BindingPattern::BindingIdentifier(ident) => {
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: ident.name.to_string(),
                        is_rest: false,
                    });
                }
            }
            ast::BindingPattern::ObjectPattern(obj_pattern) => {
                let temp_name = format!("t{}", *temp_counter);
                *temp_counter += 1;
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: temp_name.clone(),
                        is_rest: false,
                    });
                }

                // Build the destructuring pattern from the object pattern.
                // Properties with defaults are extracted into separate conditional
                // statements (e.g., {a = 2} → {a: t1} + let a = t1 === undefined ? 2 : t1).
                build_object_destructuring(
                    obj_pattern,
                    &temp_name,
                    source,
                    temp_counter,
                    &mut prefix_statements,
                );
            }
            ast::BindingPattern::ArrayPattern(arr_pattern) => {
                let temp_name = format!("t{}", *temp_counter);
                *temp_counter += 1;
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: temp_name.clone(),
                        is_rest: false,
                    });
                }

                // Build the destructuring pattern from the array pattern.
                // Elements with defaults are extracted into separate conditional
                // statements (e.g., [a = 2] → [t1] + let a = t1 === undefined ? 2 : t1).
                build_array_destructuring(
                    arr_pattern,
                    &temp_name,
                    source,
                    temp_counter,
                    &mut prefix_statements,
                );
            }
            ast::BindingPattern::AssignmentPattern(assign_pattern) => {
                // AssignmentPattern in BindingPattern is for destructuring defaults like `[a = 2]`
                let temp_name = format!("t{}", *temp_counter);
                *temp_counter += 1;
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: temp_name.clone(),
                        is_rest: false,
                    });
                }

                match &assign_pattern.left {
                    ast::BindingPattern::ObjectPattern(_)
                    | ast::BindingPattern::ArrayPattern(_)
                    | ast::BindingPattern::AssignmentPattern(_) => {
                        prefix_statements.push(compiled_prefix_statement(
                            ast::VariableDeclarationKind::Const,
                            CompiledBindingPattern::Assignment {
                                left: Box::new(compiled_binding_pattern_from_ast(
                                    &assign_pattern.left,
                                    source,
                                )),
                                default_expr: source[assign_pattern.right.span().start as usize
                                    ..assign_pattern.right.span().end as usize]
                                    .to_string(),
                            },
                            CompiledInitializer::Identifier(temp_name.clone()),
                        ))
                    }
                    ast::BindingPattern::BindingIdentifier(ident) => {
                        let default_start = assign_pattern.right.span().start as usize;
                        let default_end = assign_pattern.right.span().end as usize;
                        prefix_statements.push(compiled_prefix_statement(
                            ast::VariableDeclarationKind::Const,
                            CompiledBindingPattern::Identifier(ident.name.to_string()),
                            CompiledInitializer::UndefinedFallback {
                                temp_name: temp_name.clone(),
                                default_expr: source[default_start..default_end].to_string(),
                            },
                        ));
                    }
                }
            }
        }
    }

    // Handle rest element
    if let Some(rest) = &params.rest {
        match &rest.rest.argument {
            ast::BindingPattern::BindingIdentifier(ident) => {
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: ident.name.to_string(),
                        is_rest: true,
                    });
                }
            }
            ast::BindingPattern::ArrayPattern(arr_pattern) => {
                let temp_name = format!("t{}", *temp_counter);
                *temp_counter += 1;
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: temp_name.clone(),
                        is_rest: true,
                    });
                }
                build_array_destructuring(
                    arr_pattern,
                    &temp_name,
                    source,
                    temp_counter,
                    &mut prefix_statements,
                );
            }
            ast::BindingPattern::ObjectPattern(obj_pattern) => {
                let temp_name = format!("t{}", *temp_counter);
                *temp_counter += 1;
                if let Some(params) = compiled_params.as_mut() {
                    params.push(CompiledParam {
                        name: temp_name.clone(),
                        is_rest: true,
                    });
                }
                build_object_destructuring(
                    obj_pattern,
                    &temp_name,
                    source,
                    temp_counter,
                    &mut prefix_statements,
                );
            }
            _ => {
                compiled_params = None;
            }
        }
    }

    ParamsResult {
        compiled_params,
        prefix_statements,
        hir_outlined_functions,
    }
}

fn try_lower_default_outlined_arrow<'a>(
    name: &str,
    arrow: &ast::ArrowFunctionExpression<'a>,
    source: &'a str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
) -> Option<HIRFunction> {
    let env = crate::environment::Environment::new(options.environment.clone());
    let lowering_cx = build::LoweringContext::new(semantic, source, env);
    let mut hir_function = build::lower_arrow_expression(
        &arrow.body,
        &arrow.params,
        lowering_cx,
        build::LowerFunctionOptions::arrow(Some(name), arrow.span, arrow.r#async, arrow.expression),
    )
    .ok()?
    .func;
    hir_function.id = Some(name.to_string());
    Some(hir_function)
}

fn try_lower_default_outlined_function<'a>(
    name: &str,
    func: &ast::Function<'a>,
    source: &'a str,
    semantic: &oxc_semantic::Semantic<'a>,
    options: &PluginOptions,
) -> Option<HIRFunction> {
    let body = func.body.as_ref()?;
    let env = crate::environment::Environment::new(options.environment.clone());
    let lowering_cx = build::LoweringContext::new(semantic, source, env);
    let mut hir_function = build::lower_function(
        body,
        &func.params,
        lowering_cx,
        build::LowerFunctionOptions::function(Some(name), func.span, func.generator, func.r#async),
    )
    .ok()?
    .func;
    hir_function.id = Some(name.to_string());
    Some(hir_function)
}

/// Check if an arrow function expression used as a default parameter can be outlined.
/// The upstream outlines anonymous function expressions with no captured context.
/// We check conservatively: the arrow function must not reference any free variables
/// (no IdentifierReference nodes that could be captures from the outer scope).
fn can_outline_arrow_default(arrow: &ast::ArrowFunctionExpression, _source: &str) -> bool {
    // Collect the arrow's own parameter names
    let mut own_params: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for param in &arrow.params.items {
        collect_binding_pattern_names(&param.pattern, &mut own_params);
    }

    // Walk the body looking for identifier references.
    // If any identifier reference is NOT one of the arrow's own params and NOT a known global,
    // it might be a capture → don't outline.
    for stmt in arrow.body.statements.iter() {
        if has_non_global_references_stmt(stmt, &own_params) {
            return false;
        }
    }
    true
}

/// Check if a function expression used as a default parameter can be outlined.
fn can_outline_func_default(func: &ast::Function, _source: &str) -> bool {
    if func.id.is_some() {
        return false; // Named functions are not outlined
    }
    let mut own_params: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for param in &func.params.items {
        collect_binding_pattern_names(&param.pattern, &mut own_params);
    }
    if let Some(body) = &func.body {
        for stmt in body.statements.iter() {
            if has_non_global_references_stmt(stmt, &own_params) {
                return false;
            }
        }
    }
    true
}

/// Collect all binding names from a binding pattern.
fn collect_binding_pattern_names<'a>(
    pattern: &'a ast::BindingPattern<'a>,
    names: &mut std::collections::HashSet<&'a str>,
) {
    match pattern {
        ast::BindingPattern::BindingIdentifier(ident) => {
            names.insert(&ident.name);
        }
        ast::BindingPattern::ObjectPattern(obj) => {
            for prop in &obj.properties {
                collect_binding_pattern_names(&prop.value, names);
            }
            if let Some(rest) = &obj.rest {
                collect_binding_pattern_names(&rest.argument, names);
            }
        }
        ast::BindingPattern::ArrayPattern(arr) => {
            for elem in arr.elements.iter().flatten() {
                collect_binding_pattern_names(elem, names);
            }
            if let Some(rest) = &arr.rest {
                collect_binding_pattern_names(&rest.argument, names);
            }
        }
        ast::BindingPattern::AssignmentPattern(assign) => {
            collect_binding_pattern_names(&assign.left, names);
        }
    }
}

/// Check if a statement contains any identifier references that are NOT own params
/// and NOT known globals. Returns true if potential captures exist.
fn has_non_global_references_stmt(
    stmt: &ast::Statement,
    own_params: &std::collections::HashSet<&str>,
) -> bool {
    // Simple heuristic: walk common statement types looking for IdentifierReference nodes.
    // This is a conservative approximation — we check expressions in key positions.
    match stmt {
        ast::Statement::ExpressionStatement(expr_stmt) => {
            has_non_global_references_expr(&expr_stmt.expression, own_params)
        }
        ast::Statement::ReturnStatement(ret) => ret
            .argument
            .as_ref()
            .is_some_and(|arg| has_non_global_references_expr(arg, own_params)),
        ast::Statement::VariableDeclaration(decl) => {
            for d in &decl.declarations {
                if let Some(init) = &d.init
                    && has_non_global_references_expr(init, own_params)
                {
                    return true;
                }
            }
            false
        }
        ast::Statement::BlockStatement(block) => {
            for s in &block.body {
                if has_non_global_references_stmt(s, own_params) {
                    return true;
                }
            }
            false
        }
        ast::Statement::IfStatement(if_stmt) => {
            has_non_global_references_expr(&if_stmt.test, own_params)
                || has_non_global_references_stmt(&if_stmt.consequent, own_params)
                || if_stmt
                    .alternate
                    .as_ref()
                    .is_some_and(|alt| has_non_global_references_stmt(alt, own_params))
        }
        // For other statement types, conservatively return true
        // (they might contain captures we can't easily detect)
        ast::Statement::EmptyStatement(_) => false,
        _ => true,
    }
}

/// Check if an expression contains any identifier references that are NOT own params
/// and NOT known globals.
fn has_non_global_references_expr(
    expr: &ast::Expression,
    own_params: &std::collections::HashSet<&str>,
) -> bool {
    match expr {
        ast::Expression::Identifier(ident) => {
            let name = ident.name.as_str();
            // Known globals don't count as captures
            if own_params.contains(name) || is_known_global(name) {
                false
            } else {
                true // Potential capture
            }
        }
        ast::Expression::NumericLiteral(_)
        | ast::Expression::StringLiteral(_)
        | ast::Expression::BooleanLiteral(_)
        | ast::Expression::NullLiteral(_)
        | ast::Expression::BigIntLiteral(_)
        | ast::Expression::RegExpLiteral(_) => false,
        ast::Expression::TemplateLiteral(t) => t
            .expressions
            .iter()
            .any(|e| has_non_global_references_expr(e, own_params)),
        ast::Expression::UnaryExpression(u) => {
            has_non_global_references_expr(&u.argument, own_params)
        }
        ast::Expression::BinaryExpression(b) => {
            has_non_global_references_expr(&b.left, own_params)
                || has_non_global_references_expr(&b.right, own_params)
        }
        ast::Expression::ArrayExpression(arr) => arr.elements.iter().any(|el| match el {
            ast::ArrayExpressionElement::SpreadElement(spread) => {
                has_non_global_references_expr(&spread.argument, own_params)
            }
            ast::ArrayExpressionElement::Elision(_) => false,
            _ => has_non_global_references_expr(el.to_expression(), own_params),
        }),
        ast::Expression::ObjectExpression(obj) => obj.properties.iter().any(|prop| match prop {
            ast::ObjectPropertyKind::ObjectProperty(p) => {
                has_non_global_references_expr(&p.value, own_params)
            }
            ast::ObjectPropertyKind::SpreadProperty(s) => {
                has_non_global_references_expr(&s.argument, own_params)
            }
        }),
        ast::Expression::CallExpression(call) => {
            has_non_global_references_expr(&call.callee, own_params)
                || call.arguments.iter().any(|arg| match arg {
                    ast::Argument::SpreadElement(s) => {
                        has_non_global_references_expr(&s.argument, own_params)
                    }
                    _ => has_non_global_references_expr(arg.to_expression(), own_params),
                })
        }
        ast::Expression::StaticMemberExpression(m) => {
            has_non_global_references_expr(&m.object, own_params)
        }
        ast::Expression::ComputedMemberExpression(m) => {
            has_non_global_references_expr(&m.object, own_params)
                || has_non_global_references_expr(&m.expression, own_params)
        }
        ast::Expression::ConditionalExpression(c) => {
            has_non_global_references_expr(&c.test, own_params)
                || has_non_global_references_expr(&c.consequent, own_params)
                || has_non_global_references_expr(&c.alternate, own_params)
        }
        ast::Expression::LogicalExpression(l) => {
            has_non_global_references_expr(&l.left, own_params)
                || has_non_global_references_expr(&l.right, own_params)
        }
        ast::Expression::SequenceExpression(s) => s
            .expressions
            .iter()
            .any(|e| has_non_global_references_expr(e, own_params)),
        ast::Expression::ParenthesizedExpression(p) => {
            has_non_global_references_expr(&p.expression, own_params)
        }
        // Arrow/function expressions are self-contained — they don't contribute captures
        ast::Expression::ArrowFunctionExpression(_) | ast::Expression::FunctionExpression(_) => {
            false
        }
        // For other expression types, conservatively return true
        _ => true,
    }
}

/// Check if a name is a known JavaScript global.
fn is_known_global(name: &str) -> bool {
    matches!(
        name,
        "undefined"
            | "null"
            | "true"
            | "false"
            | "NaN"
            | "Infinity"
            | "Math"
            | "JSON"
            | "console"
            | "window"
            | "document"
            | "globalThis"
            | "Object"
            | "Array"
            | "String"
            | "Number"
            | "Boolean"
            | "Symbol"
            | "Map"
            | "Set"
            | "WeakMap"
            | "WeakSet"
            | "Promise"
            | "RegExp"
            | "Error"
            | "TypeError"
            | "RangeError"
            | "SyntaxError"
            | "ReferenceError"
            | "Date"
            | "parseInt"
            | "parseFloat"
            | "isNaN"
            | "isFinite"
            | "encodeURI"
            | "decodeURI"
            | "encodeURIComponent"
            | "decodeURIComponent"
            | "setTimeout"
            | "setInterval"
            | "clearTimeout"
            | "clearInterval"
            | "alert"
            | "confirm"
            | "prompt"
            | "fetch"
            | "URL"
            | "URLSearchParams"
            | "Proxy"
            | "Reflect"
            | "BigInt"
            | "ArrayBuffer"
            | "SharedArrayBuffer"
            | "DataView"
            | "Float32Array"
            | "Float64Array"
            | "Int8Array"
            | "Int16Array"
            | "Int32Array"
            | "Uint8Array"
            | "Uint16Array"
            | "Uint32Array"
            | "Uint8ClampedArray"
            | "Intl"
            | "Atomics"
            | "WebAssembly"
            | "queueMicrotask"
            | "structuredClone"
            | "performance"
            | "crypto"
            | "navigator"
            | "location"
            | "history"
            | "process"
            | "require"
            | "module"
            | "exports"
            | "__dirname"
            | "__filename"
            | "Buffer"
            | "Event"
            | "CustomEvent"
            | "EventTarget"
    )
}

/// Build a destructuring statement from an object pattern.
/// e.g., `const { a, b, c: d } = t0;`
/// Properties with defaults (e.g., `{a = 2}`) are extracted into separate
/// conditional statements: `{a: t1} = t0; let a = t1 === undefined ? 2 : t1;`
fn build_object_destructuring(
    pattern: &ast::ObjectPattern,
    temp_name: &str,
    source: &str,
    temp_counter: &mut usize,
    prefix_statements: &mut Vec<CompiledParamPrefixStatement>,
) {
    let base_index = prefix_statements.len();
    let mut properties = Vec::new();
    for prop in &pattern.properties {
        match &prop.value {
            ast::BindingPattern::BindingIdentifier(ident) => {
                properties.push(CompiledObjectPatternProperty {
                    key: compiled_property_key_from_ast(&prop.key, source),
                    value: CompiledBindingPattern::Identifier(ident.name.to_string()),
                    shorthand: !prop.computed
                        && matches!(
                            &prop.key,
                            ast::PropertyKey::StaticIdentifier(id) if id.name == ident.name
                        ),
                    computed: prop.computed,
                });
            }
            ast::BindingPattern::AssignmentPattern(assign) => {
                if let ast::BindingPattern::BindingIdentifier(ident) = &assign.left {
                    let elem_temp = format!("t{}", *temp_counter);
                    *temp_counter += 1;
                    let default_start = assign.right.span().start as usize;
                    let default_end = assign.right.span().end as usize;
                    properties.push(CompiledObjectPatternProperty {
                        key: compiled_property_key_from_ast(&prop.key, source),
                        value: CompiledBindingPattern::Identifier(elem_temp.clone()),
                        shorthand: false,
                        computed: prop.computed,
                    });
                    prefix_statements.push(compiled_prefix_statement(
                        ast::VariableDeclarationKind::Let,
                        CompiledBindingPattern::Identifier(ident.name.to_string()),
                        CompiledInitializer::UndefinedFallback {
                            temp_name: elem_temp,
                            default_expr: source[default_start..default_end].to_string(),
                        },
                    ));
                } else {
                    properties.push(CompiledObjectPatternProperty {
                        key: compiled_property_key_from_ast(&prop.key, source),
                        value: CompiledBindingPattern::Assignment {
                            left: Box::new(compiled_binding_pattern_from_ast(&assign.left, source)),
                            default_expr: source[assign.right.span().start as usize
                                ..assign.right.span().end as usize]
                                .to_string(),
                        },
                        shorthand: false,
                        computed: prop.computed,
                    });
                }
            }
            _ => {
                properties.push(CompiledObjectPatternProperty {
                    key: compiled_property_key_from_ast(&prop.key, source),
                    value: compiled_binding_pattern_from_ast(&prop.value, source),
                    shorthand: false,
                    computed: prop.computed,
                });
            }
        }
    }

    prefix_statements.insert(
        base_index,
        compiled_prefix_statement(
            ast::VariableDeclarationKind::Let,
            CompiledBindingPattern::Object(CompiledObjectPattern {
                properties,
                rest: pattern.rest.as_ref().map(|rest| {
                    Box::new(compiled_binding_pattern_from_ast(&rest.argument, source))
                }),
            }),
            CompiledInitializer::Identifier(temp_name.to_string()),
        ),
    );
}

/// Build a destructuring statement from an array pattern.
fn build_array_destructuring(
    pattern: &ast::ArrayPattern,
    temp_name: &str,
    source: &str,
    temp_counter: &mut usize,
    prefix_statements: &mut Vec<CompiledParamPrefixStatement>,
) {
    let base_index = prefix_statements.len();
    let mut elements = Vec::new();

    for elem in &pattern.elements {
        match elem {
            Some(ast::BindingPattern::BindingIdentifier(ident)) => {
                elements.push(Some(CompiledBindingPattern::Identifier(
                    ident.name.to_string(),
                )));
            }
            Some(ast::BindingPattern::AssignmentPattern(assign)) => {
                let elem_temp = format!("t{}", *temp_counter);
                *temp_counter += 1;
                elements.push(Some(CompiledBindingPattern::Identifier(elem_temp.clone())));
                let default_start = assign.right.span().start as usize;
                let default_end = assign.right.span().end as usize;
                prefix_statements.push(compiled_prefix_statement(
                    ast::VariableDeclarationKind::Let,
                    compiled_binding_pattern_from_ast(&assign.left, source),
                    CompiledInitializer::UndefinedFallback {
                        temp_name: elem_temp,
                        default_expr: source[default_start..default_end].to_string(),
                    },
                ));
            }
            Some(binding) => {
                elements.push(Some(compiled_binding_pattern_from_ast(binding, source)));
            }
            None => elements.push(None),
        }
    }

    prefix_statements.insert(
        base_index,
        compiled_prefix_statement(
            ast::VariableDeclarationKind::Let,
            CompiledBindingPattern::Array(CompiledArrayPattern {
                elements,
                rest: pattern.rest.as_ref().map(|rest| {
                    Box::new(compiled_binding_pattern_from_ast(&rest.argument, source))
                }),
            }),
            CompiledInitializer::Identifier(temp_name.to_string()),
        ),
    );
}

fn compiled_prefix_statement(
    kind: ast::VariableDeclarationKind,
    pattern: CompiledBindingPattern,
    init: CompiledInitializer,
) -> CompiledParamPrefixStatement {
    CompiledParamPrefixStatement {
        kind,
        pattern,
        init,
    }
}

fn compiled_binding_pattern_from_ast(
    pattern: &ast::BindingPattern<'_>,
    source: &str,
) -> CompiledBindingPattern {
    match pattern {
        ast::BindingPattern::BindingIdentifier(ident) => {
            CompiledBindingPattern::Identifier(ident.name.to_string())
        }
        ast::BindingPattern::ObjectPattern(object) => {
            let properties = object
                .properties
                .iter()
                .map(|prop| CompiledObjectPatternProperty {
                    key: compiled_property_key_from_ast(&prop.key, source),
                    value: compiled_binding_pattern_from_ast(&prop.value, source),
                    shorthand: prop.shorthand,
                    computed: prop.computed,
                })
                .collect();
            CompiledBindingPattern::Object(CompiledObjectPattern {
                properties,
                rest: object.rest.as_ref().map(|rest| {
                    Box::new(compiled_binding_pattern_from_ast(&rest.argument, source))
                }),
            })
        }
        ast::BindingPattern::ArrayPattern(array) => {
            CompiledBindingPattern::Array(CompiledArrayPattern {
                elements: array
                    .elements
                    .iter()
                    .map(|element| {
                        element
                            .as_ref()
                            .map(|pattern| compiled_binding_pattern_from_ast(pattern, source))
                    })
                    .collect(),
                rest: array.rest.as_ref().map(|rest| {
                    Box::new(compiled_binding_pattern_from_ast(&rest.argument, source))
                }),
            })
        }
        ast::BindingPattern::AssignmentPattern(assign) => CompiledBindingPattern::Assignment {
            left: Box::new(compiled_binding_pattern_from_ast(&assign.left, source)),
            default_expr: source
                [assign.right.span().start as usize..assign.right.span().end as usize]
                .to_string(),
        },
    }
}

fn compiled_property_key_from_ast(key: &ast::PropertyKey<'_>, source: &str) -> CompiledPropertyKey {
    match key {
        ast::PropertyKey::StaticIdentifier(id) => {
            CompiledPropertyKey::StaticIdentifier(id.name.to_string())
        }
        ast::PropertyKey::StringLiteral(string) => {
            CompiledPropertyKey::StringLiteral(string.value.to_string())
        }
        _ => {
            let start = key.span().start as usize;
            let end = key.span().end as usize;
            CompiledPropertyKey::Source(source[start..end].to_string())
        }
    }
}

// is_component_name and is_hook_name are defined at the top of the file.

/// Validates that the HIR does not call unsupported global functions like `eval`.
///
/// Port of the eval check from upstream BuildHIR.ts:3681.
/// The `eval` function is not supported because the code it executes cannot be
/// analyzed by React Compiler.
fn validate_no_unsupported_global_calls(func: &HIRFunction) -> Result<(), CompilerError> {
    use crate::error::{BailOut, CompilerDiagnostic, DiagnosticSeverity};
    use crate::hir::types::{IdentifierId, InstructionValue};
    use std::collections::HashSet;
    // First pass: collect identifier IDs that are LoadGlobal for unsupported globals
    let mut unsupported_global_ids: HashSet<IdentifierId> = HashSet::new();
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::LoadGlobal { binding, .. } = &instr.value
                && binding.name() == "eval"
            {
                unsupported_global_ids.insert(instr.lvalue.identifier.id);
            }
        }
    }
    if unsupported_global_ids.is_empty() {
        return Ok(());
    }
    // Second pass: check if any CallExpression uses an unsupported global as callee
    for (_bid, block) in &func.body.blocks {
        for instr in &block.instructions {
            if let InstructionValue::CallExpression { callee, .. } = &instr.value
                && unsupported_global_ids.contains(&callee.identifier.id)
            {
                return Err(CompilerError::Bail(BailOut {
                    reason: "Unsupported global function call".to_string(),
                    diagnostics: vec![CompilerDiagnostic {
                        severity: DiagnosticSeverity::InvalidReact,
                        message: "The 'eval' function is not supported. \
                                     It is an anti-pattern in JavaScript, and the code executed \
                                     cannot be analyzed by React Compiler."
                            .to_string(),
                    }],
                }));
            }
        }
    }
    Ok(())
}

/// Validates that a function body does not dynamically create components or hooks.
///
/// Port of `validateNoDynamicallyCreatedComponentsOrHooks` from upstream Program.ts:869-928.
/// When enabled via `@validateNoDynamicallyCreatedComponentsOrHooks`, this check ensures that
/// React components and hooks are always declared at module scope, preventing scope reference
/// errors during compilation.
fn validate_no_dynamic_components_or_hooks(
    body: &ast::FunctionBody<'_>,
    parent_name: &str,
) -> Result<(), CompilerError> {
    use crate::error::{BailOut, CompilerDiagnostic, DiagnosticSeverity};

    fn check_name(name: &str, parent_name: &str) -> Option<CompilerDiagnostic> {
        let fn_type = if is_component_name(name) {
            "component"
        } else if is_hook_name(name) {
            "hook"
        } else {
            return None;
        };
        Some(CompilerDiagnostic {
            severity: DiagnosticSeverity::InvalidReact,
            message: format!(
                "Components and hooks cannot be created dynamically. \
                 The function `{name}` appears to be a React {fn_type}, \
                 but it's defined inside `{parent_name}`. \
                 Components and Hooks should always be declared at module scope",
            ),
        })
    }

    fn walk_stmt(
        stmt: &ast::Statement<'_>,
        parent_name: &str,
        diags: &mut Vec<CompilerDiagnostic>,
    ) {
        match stmt {
            ast::Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id
                    && let Some(d) = check_name(id.name.as_str(), parent_name)
                {
                    diags.push(d);
                    return;
                }
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_stmt(s, parent_name, diags);
                    }
                }
            }
            ast::Statement::VariableDeclaration(decl) => {
                for declarator in &decl.declarations {
                    if let Some(init) = &declarator.init {
                        let is_fn = matches!(
                            init,
                            ast::Expression::ArrowFunctionExpression(_)
                                | ast::Expression::FunctionExpression(_)
                        );
                        if is_fn
                            && let ast::BindingPattern::BindingIdentifier(id) = &declarator.id
                            && let Some(d) = check_name(id.name.as_str(), parent_name)
                        {
                            diags.push(d);
                            continue;
                        }
                        walk_expr(init, parent_name, diags);
                    }
                }
            }
            ast::Statement::BlockStatement(block) => {
                for s in &block.body {
                    walk_stmt(s, parent_name, diags);
                }
            }
            ast::Statement::IfStatement(if_stmt) => {
                walk_stmt(&if_stmt.consequent, parent_name, diags);
                if let Some(alt) = &if_stmt.alternate {
                    walk_stmt(alt, parent_name, diags);
                }
            }
            ast::Statement::ForStatement(for_stmt) => {
                walk_stmt(&for_stmt.body, parent_name, diags);
            }
            ast::Statement::ForInStatement(for_in) => {
                walk_stmt(&for_in.body, parent_name, diags);
            }
            ast::Statement::ForOfStatement(for_of) => {
                walk_stmt(&for_of.body, parent_name, diags);
            }
            ast::Statement::WhileStatement(while_stmt) => {
                walk_stmt(&while_stmt.body, parent_name, diags);
            }
            ast::Statement::DoWhileStatement(do_while) => {
                walk_stmt(&do_while.body, parent_name, diags);
            }
            ast::Statement::TryStatement(try_stmt) => {
                for s in &try_stmt.block.body {
                    walk_stmt(s, parent_name, diags);
                }
                if let Some(handler) = &try_stmt.handler {
                    for s in &handler.body.body {
                        walk_stmt(s, parent_name, diags);
                    }
                }
                if let Some(finalizer) = &try_stmt.finalizer {
                    for s in &finalizer.body {
                        walk_stmt(s, parent_name, diags);
                    }
                }
            }
            ast::Statement::SwitchStatement(switch) => {
                for case in &switch.cases {
                    for s in &case.consequent {
                        walk_stmt(s, parent_name, diags);
                    }
                }
            }
            ast::Statement::ExpressionStatement(expr_stmt) => {
                walk_expr(&expr_stmt.expression, parent_name, diags);
            }
            ast::Statement::ReturnStatement(ret) => {
                if let Some(arg) = &ret.argument {
                    walk_expr(arg, parent_name, diags);
                }
            }
            _ => {}
        }
    }

    fn walk_expr(
        expr: &ast::Expression<'_>,
        parent_name: &str,
        diags: &mut Vec<CompilerDiagnostic>,
    ) {
        match expr {
            ast::Expression::FunctionExpression(func) => {
                if let Some(id) = &func.id
                    && let Some(d) = check_name(id.name.as_str(), parent_name)
                {
                    diags.push(d);
                }
            }
            ast::Expression::CallExpression(call) => {
                for arg in &call.arguments {
                    if let ast::Argument::SpreadElement(_) = arg {
                        continue;
                    }
                    // Safety: Argument variants other than SpreadElement are Expression-like
                    let arg_expr: &ast::Expression<'_> = unsafe { std::mem::transmute(arg) };
                    walk_expr(arg_expr, parent_name, diags);
                }
            }
            ast::Expression::AssignmentExpression(assign) => {
                walk_expr(&assign.right, parent_name, diags);
            }
            _ => {}
        }
    }

    let mut diags = Vec::new();
    for stmt in &body.statements {
        walk_stmt(stmt, parent_name, &mut diags);
    }

    if diags.is_empty() {
        Ok(())
    } else {
        Err(CompilerError::Bail(BailOut {
            reason: "Components and hooks cannot be created dynamically".to_string(),
            diagnostics: diags,
        }))
    }
}

/// Program-level validation for dynamically created components/hooks.
///
/// Walks ALL functions in the program (not just compilable ones) and checks
/// if any of them define nested functions with component or hook names.
fn validate_no_dynamic_components_or_hooks_program(
    program: &ast::Program<'_>,
) -> Result<(), CompilerError> {
    for stmt in &program.body {
        check_stmt_for_dynamic_components(stmt)?
    }
    Ok(())
}

fn check_stmt_for_dynamic_components(stmt: &ast::Statement<'_>) -> Result<(), CompilerError> {
    match stmt {
        ast::Statement::FunctionDeclaration(func) => {
            let name = func
                .id
                .as_ref()
                .map(|id| id.name.as_str())
                .unwrap_or("<anonymous>");
            if let Some(body) = &func.body {
                validate_no_dynamic_components_or_hooks(body, name)?;
            }
        }
        ast::Statement::ExportDefaultDeclaration(export) => {
            if let ast::ExportDefaultDeclarationKind::FunctionDeclaration(func) =
                &export.declaration
            {
                let name = func
                    .id
                    .as_ref()
                    .map(|id| id.name.as_str())
                    .unwrap_or("<anonymous>");
                if let Some(body) = &func.body {
                    validate_no_dynamic_components_or_hooks(body, name)?;
                }
            }
        }
        ast::Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                match decl {
                    ast::Declaration::FunctionDeclaration(func) => {
                        let name = func
                            .id
                            .as_ref()
                            .map(|id| id.name.as_str())
                            .unwrap_or("<anonymous>");
                        if let Some(body) = &func.body {
                            validate_no_dynamic_components_or_hooks(body, name)?;
                        }
                    }
                    ast::Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(init) = &declarator.init {
                                check_var_init_for_dynamic_components(&declarator.id, init)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        ast::Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(init) = &declarator.init {
                    check_var_init_for_dynamic_components(&declarator.id, init)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_var_init_for_dynamic_components(
    id: &ast::BindingPattern<'_>,
    init: &ast::Expression<'_>,
) -> Result<(), CompilerError> {
    let name = match id {
        ast::BindingPattern::BindingIdentifier(ident) => ident.name.as_str(),
        _ => return Ok(()),
    };
    match init {
        ast::Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                validate_no_dynamic_components_or_hooks(body, name)?;
            }
        }
        ast::Expression::ArrowFunctionExpression(arrow) => {
            validate_no_dynamic_components_or_hooks(&arrow.body, name)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use oxc_allocator::Allocator;
    use oxc_ast::ast;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    use crate::options::PluginOptions;

    use super::{align_params_result_with_codegen, extract_emitted_directives, params_to_result};

    #[test]
    fn params_to_result_collects_hir_for_outlined_default_arrow() {
        let allocator = Allocator::default();
        let source = "function Component(callback = () => {}) { return callback; }";
        let parser_ret = Parser::new(&allocator, source, SourceType::mjs()).parse();
        assert!(
            parser_ret.errors.is_empty(),
            "parse errors: {:?}",
            parser_ret.errors
        );
        let program = parser_ret.program;
        let semantic_ret = SemanticBuilder::new().build(&program);
        let semantic = semantic_ret.semantic;
        let ast::Statement::FunctionDeclaration(function) = &program.body[0] else {
            panic!("expected function declaration");
        };

        let mut temp_counter = 0;
        let params_result = params_to_result(
            &function.params,
            source,
            &semantic,
            &PluginOptions::default(),
            &mut temp_counter,
        );

        assert_eq!(params_result.hir_outlined_functions.len(), 1);
        assert_eq!(params_result.hir_outlined_functions[0].0, "_temp");
        assert_eq!(
            params_result.hir_outlined_functions[0].1.id.as_deref(),
            Some("_temp")
        );
    }

    #[test]
    fn align_params_result_with_codegen_renames_rest_identifier_params() {
        let allocator = Allocator::default();
        let source = "function Component(foo, ...bar) { return [foo, bar]; }";
        let parser_ret = Parser::new(&allocator, source, SourceType::mjs()).parse();
        assert!(
            parser_ret.errors.is_empty(),
            "parse errors: {:?}",
            parser_ret.errors
        );
        let program = parser_ret.program;
        let semantic_ret = SemanticBuilder::new().build(&program);
        let semantic = semantic_ret.semantic;
        let ast::Statement::FunctionDeclaration(function) = &program.body[0] else {
            panic!("expected function declaration");
        };

        let mut temp_counter = 0;
        let mut params_result = params_to_result(
            &function.params,
            source,
            &semantic,
            &PluginOptions::default(),
            &mut temp_counter,
        );
        align_params_result_with_codegen(
            &mut params_result,
            &["foo".to_string(), "t0".to_string()],
        );

        let compiled_params = params_result
            .compiled_params
            .expect("expected compiled params");
        assert_eq!(compiled_params[0].name, "foo");
        assert_eq!(compiled_params[1].name, "t0");
        assert!(compiled_params[1].is_rest);
    }

    #[test]
    fn align_params_result_with_codegen_rewrites_prefix_statement_temps() {
        let allocator = Allocator::default();
        let source = "function Component({x}) { return x; }";
        let parser_ret = Parser::new(&allocator, source, SourceType::mjs()).parse();
        assert!(
            parser_ret.errors.is_empty(),
            "parse errors: {:?}",
            parser_ret.errors
        );
        let program = parser_ret.program;
        let semantic_ret = SemanticBuilder::new().build(&program);
        let semantic = semantic_ret.semantic;
        let ast::Statement::FunctionDeclaration(function) = &program.body[0] else {
            panic!("expected function declaration");
        };

        let mut temp_counter = 0;
        let mut params_result = params_to_result(
            &function.params,
            source,
            &semantic,
            &PluginOptions::default(),
            &mut temp_counter,
        );
        align_params_result_with_codegen(&mut params_result, &["t1".to_string()]);

        let compiled_params = params_result
            .compiled_params
            .as_ref()
            .expect("expected compiled params");
        assert_eq!(compiled_params[0].name, "t1");
        assert_eq!(params_result.prefix_statements.len(), 1);
        match &params_result.prefix_statements[0].init {
            crate::codegen_backend::CompiledInitializer::Identifier(name) => {
                assert_eq!(name, "t1");
            }
            other => panic!("expected identifier initializer, got {other:?}"),
        }
    }

    #[test]
    fn extract_emitted_directives_keeps_ignored_opt_outs() {
        let allocator = Allocator::default();
        let source = "function Component() { 'use no forget'; 'use memo'; return 1; }";
        let parser_ret = Parser::new(&allocator, source, SourceType::mjs()).parse();
        assert!(
            parser_ret.errors.is_empty(),
            "parse errors: {:?}",
            parser_ret.errors
        );
        let program = parser_ret.program;
        let ast::Statement::FunctionDeclaration(function) = &program.body[0] else {
            panic!("expected function declaration");
        };
        let body = function.body.as_ref().expect("expected function body");

        assert_eq!(
            extract_emitted_directives(body),
            vec!["\"use no forget\"", "\"use memo\""]
        );
    }
}
