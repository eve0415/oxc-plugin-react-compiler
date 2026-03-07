use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::CompileResult;

use super::{CompiledFunction, ModuleEmitArgs};

pub(crate) fn emit_module(
    args: ModuleEmitArgs<'_>,
    compiled: Vec<CompiledFunction>,
) -> CompileResult {
    let raw_result = super::raw::emit_module(args, compiled);
    if !raw_result.transformed {
        return raw_result;
    }

    match reprint_with_oxc_codegen(args.filename, &raw_result.code) {
        Ok(code) => CompileResult {
            transformed: raw_result.transformed,
            code,
            map: raw_result.map,
        },
        Err(_) => raw_result,
    }
}

fn reprint_with_oxc_codegen(filename: &str, source: &str) -> Result<String, String> {
    let allocator = Allocator::default();
    let source_type = source_type_for_filename(filename);
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked || !parsed.errors.is_empty() {
        return Err(format!(
            "failed to parse generated source for ast backend: {} errors",
            parsed.errors.len()
        ));
    }

    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        ..CodegenOptions::default()
    };
    Ok(Codegen::new().with_options(options).build(&parsed.program).code)
}

fn source_type_for_filename(filename: &str) -> SourceType {
    if filename.ends_with(".tsx") {
        SourceType::tsx()
    } else if filename.ends_with(".ts") {
        SourceType::ts().with_jsx(true)
    } else if filename.ends_with(".jsx") {
        SourceType::jsx()
    } else {
        SourceType::mjs().with_jsx(true)
    }
}

#[cfg(test)]
mod tests {
    use super::reprint_with_oxc_codegen;

    #[test]
    fn reparses_and_reprints_jsx_output() {
        let source = "function Component() {\n  return <div>Hello</div>;\n}\n";
        let result = reprint_with_oxc_codegen("Component.jsx", source).unwrap();
        assert!(result.contains("return <div>Hello</div>;"));
    }
}
