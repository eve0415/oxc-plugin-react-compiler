//! Shared test utilities for `oxc_react_compiler` unit tests.
#![allow(dead_code, unused_imports)]
use crate::environment::Environment;
use crate::hir::types::HIRFunction;
use crate::options::EnvironmentConfig;
pub fn parse_and_lower(source: &str) -> Result<HIRFunction, String> {
    let wrapped;
    let effective_source = if source.starts_with("function ") || source.starts_with("export ") {
        source
    } else {
        wrapped = format!("function Component(props) {{ {source} }}");
        &wrapped
    };
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::mjs().with_jsx(true);
    let parser_ret = oxc_parser::Parser::new(&allocator, effective_source, source_type).parse();
    if !parser_ret.errors.is_empty() {
        return Err(format!("parse errors: {:?}", parser_ret.errors));
    }
    let semantic_ret = oxc_semantic::SemanticBuilder::new().build(&parser_ret.program);
    let semantic = semantic_ret.semantic;
    let func = parser_ret
        .program
        .body
        .iter()
        .find_map(|stmt| match stmt {
            oxc_ast::ast::Statement::FunctionDeclaration(f) => Some(f.as_ref()),
            oxc_ast::ast::Statement::ExportNamedDeclaration(export) => {
                export.declaration.as_ref().and_then(|d| match d {
                    oxc_ast::ast::Declaration::FunctionDeclaration(f) => Some(f.as_ref()),
                    _ => None,
                })
            }
            _ => None,
        })
        .ok_or("no function declaration found")?;
    let body = func.body.as_ref().ok_or("function has no body")?;
    let cx = crate::hir::build::LoweringContext::new(
        &semantic,
        effective_source,
        Environment::new(EnvironmentConfig::default()),
    );
    let result = crate::hir::build::lower_function(
        body,
        &func.params,
        cx,
        crate::hir::build::LowerFunctionOptions::function(
            func.id.as_ref().map(|id| id.name.as_str()),
            func.span,
            func.generator,
            func.r#async,
        ),
    )?;
    Ok(result.func)
}
#[allow(dead_code)]
pub fn compile_to_result(source: &str) -> crate::CompileResult {
    let options = crate::options::PluginOptions::default();
    crate::compile("test.jsx", source, &options)
}
