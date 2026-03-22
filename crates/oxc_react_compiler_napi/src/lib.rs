//! N-API bindings for oxc_react_compiler.

use napi_derive::napi;

#[napi(object)]
pub struct TransformOptions {
    #[napi(ts_type = "'infer' | 'annotation' | 'all'")]
    pub compilation_mode: Option<String>,
    #[napi(ts_type = "'none' | 'all'")]
    pub panic_threshold: Option<String>,
    pub target: Option<String>,
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
    }
}
