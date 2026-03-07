fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";

    let path = format!("{}/useMemo-named-function.ts", fixture_dir);
    let source = std::fs::read_to_string(&path).unwrap();

    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::tsx();
    let parser_ret = oxc_parser::Parser::new(&allocator, &source, source_type).parse();
    let semantic_ret = oxc_semantic::SemanticBuilder::new().build(&parser_ret.program);
    let semantic = semantic_ret.semantic;

    // Find the function
    for stmt in &parser_ret.program.body {
        if let oxc_ast::ast::Statement::FunctionDeclaration(func) = stmt {
            let name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("?");
            eprintln!("=== Function: {} ===", name);

            let body = func.body.as_ref().unwrap();
            let lower_result = oxc_react_compiler::hir::build::lower_function(
                body,
                &func.params,
                oxc_react_compiler::hir::build::LoweringContext::new(
                    &semantic,
                    &source,
                    oxc_react_compiler::environment::Environment::new(
                        oxc_react_compiler::options::EnvironmentConfig::default(),
                    ),
                ),
                oxc_react_compiler::hir::build::LowerFunctionOptions::function(
                    Some(name),
                    func.span,
                    func.generator,
                    func.r#async,
                ),
            )
            .unwrap();

            let hir_func = &lower_result.func;
            for (block_id, block) in &hir_func.body.blocks {
                eprintln!("Block {:?}:", block_id);
                for instr in &block.instructions {
                    eprintln!("  {:?}: {:?}", instr.lvalue.identifier.id, instr.value);
                }
            }
        }
    }
}
