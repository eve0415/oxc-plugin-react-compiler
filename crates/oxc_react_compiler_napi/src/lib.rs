//! N-API bindings for oxc_react_compiler.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

use napi_derive::napi;

// ── Transform API (existing) ──────────────────────────────────────

#[napi(object)]
pub struct TransformOptions {
    #[napi(ts_type = "'infer' | 'annotation' | 'all'")]
    pub compilation_mode: Option<String>,
    #[napi(ts_type = "'none' | 'all'")]
    pub panic_threshold: Option<String>,
    pub target: Option<String>,
    /// Whether to generate source maps. Defaults to `true`.
    pub source_map: Option<bool>,
}

#[napi(object)]
pub struct TransformResult {
    pub transformed: bool,
    pub code: String,
    pub map: Option<String>,
}

#[napi]
pub fn transform(
    filename: String,
    source: String,
    options: Option<TransformOptions>,
) -> TransformResult {
    let opts = parse_options(options);
    let result = oxc_react_compiler::compile(&filename, &source, &opts);
    TransformResult {
        transformed: result.transformed,
        code: result.code,
        map: result.map,
    }
}

fn parse_options(options: Option<TransformOptions>) -> oxc_react_compiler::options::PluginOptions {
    let Some(opts) = options else {
        return oxc_react_compiler::options::PluginOptions::default();
    };

    use oxc_react_compiler::options::*;

    let compilation_mode = match opts.compilation_mode.as_deref() {
        Some("annotation") => CompilationMode::Annotation,
        Some("all") => CompilationMode::All,
        _ => CompilationMode::Infer,
    };

    let panic_threshold = match opts.panic_threshold.as_deref() {
        Some("all") => PanicThreshold::All,
        _ => PanicThreshold::None,
    };

    PluginOptions {
        compilation_mode,
        panic_threshold,
        target: opts.target.unwrap_or_else(|| "19".to_string()),
        environment: EnvironmentConfig::default(),
        custom_opt_out_directives: Vec::new(),
        ignore_use_no_forget: false,
        gating: None,
        dynamic_gating: None,
        no_emit: false,
        eslint_suppression_rules: None,
        flow_suppressions: true,
        source_map: opts.source_map.unwrap_or(true),
    }
}

// ── Lint API ──────────────────────────────────────────────────────

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintRelated {
    pub message: String,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintSuggestion {
    pub description: String,
    /// "insert-before" | "insert-after" | "remove" | "replace"
    pub op: String,
    pub range_start: u32,
    pub range_end: u32,
    pub text: Option<String>,
}

#[napi(object)]
#[derive(Clone)]
pub struct NapiLintDiagnostic {
    /// ErrorCategory string value (e.g., "Hooks", "Purity")
    pub category: String,
    pub message: String,
    /// "error" | "warning" | "hint"
    pub severity: String,
    pub start_line: Option<u32>,
    pub start_column: Option<u32>,
    pub end_line: Option<u32>,
    pub end_column: Option<u32>,
    pub related: Vec<NapiLintRelated>,
    pub suggestions: Vec<NapiLintSuggestion>,
}

// ── LRU Cache ─────────────────────────────────────────────────────

const LINT_CACHE_SIZE: usize = 10;

thread_local! {
    static LINT_CACHE: RefCell<VecDeque<(u64, Vec<NapiLintDiagnostic>)>> =
        RefCell::new(VecDeque::with_capacity(LINT_CACHE_SIZE));
}

fn compute_cache_key(filename: &str, source: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    filename.hash(&mut hasher);
    source.hash(&mut hasher);
    hasher.finish()
}

fn cache_get(key: u64) -> Option<Vec<NapiLintDiagnostic>> {
    LINT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let pos = cache.iter().position(|(k, _)| *k == key)?;
        let entry = cache.remove(pos)?;
        let result = entry.1.clone();
        cache.push_back(entry);
        Some(result)
    })
}

fn cache_insert(key: u64, diagnostics: Vec<NapiLintDiagnostic>) {
    LINT_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= LINT_CACHE_SIZE {
            cache.pop_front();
        }
        cache.push_back((key, diagnostics));
    });
}

// ── Conversion helpers ────────────────────────────────────────────

fn convert_diagnostic(diag: oxc_react_compiler::error::LintDiagnostic) -> NapiLintDiagnostic {
    NapiLintDiagnostic {
        category: format!("{:?}", diag.category),
        message: diag.message,
        severity: diag.severity.to_string(),
        start_line: if diag.has_location {
            Some(diag.start_line)
        } else {
            None
        },
        start_column: if diag.has_location {
            Some(diag.start_column)
        } else {
            None
        },
        end_line: if diag.has_location {
            Some(diag.end_line)
        } else {
            None
        },
        end_column: if diag.has_location {
            Some(diag.end_column)
        } else {
            None
        },
        related: diag
            .related
            .into_iter()
            .map(|r| NapiLintRelated {
                message: r.message,
                start_line: r.start_line,
                start_column: r.start_column,
                end_line: r.end_line,
                end_column: r.end_column,
            })
            .collect(),
        suggestions: diag
            .suggestions
            .into_iter()
            .map(|s| NapiLintSuggestion {
                description: s.description,
                op: s.op.to_string(),
                range_start: s.range.0,
                range_end: s.range.1,
                text: s.text,
            })
            .collect(),
    }
}

#[napi]
pub fn lint(filename: String, source: String) -> Vec<NapiLintDiagnostic> {
    let cache_key = compute_cache_key(&filename, &source);

    if let Some(cached) = cache_get(cache_key) {
        return cached;
    }

    let diagnostics = oxc_react_compiler::lint(&filename, &source);
    let result: Vec<NapiLintDiagnostic> = diagnostics.into_iter().map(convert_diagnostic).collect();

    cache_insert(cache_key, result.clone());
    result
}
