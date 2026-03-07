fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

    // Look at "object-values" specifically
    for entry in std::fs::read_dir(fixture_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        if stem != "object-values" {
            continue;
        }

        let source = std::fs::read_to_string(&path).unwrap();
        eprintln!("=== SOURCE ===");
        eprintln!("{}", source);

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        // Need to build the HIR and dump it
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::jsx();
        let parser_ret = oxc_parser::Parser::new(&allocator, source.as_str(), source_type).parse();
        let semantic = oxc_semantic::SemanticBuilder::new()
            .build(&parser_ret.program)
            .semantic;

        // Find the component function and lower it
        for stmt in &parser_ret.program.body {
            if let oxc_ast::ast::Statement::FunctionDeclaration(func) = stmt {
                let name = func.id.as_ref().map(|id| id.name.as_str());
                eprintln!("\n=== Function: {:?} ===", name);
                if let Some(body) = func.body.as_ref() {
                    let lower_result = oxc_react_compiler::hir::build::lower_function(
                        body,
                        &func.params,
                        name,
                        func.span,
                        func.generator,
                        func.r#async,
                        &semantic,
                        &source,
                        oxc_react_compiler::environment::Environment::new(
                            oxc_react_compiler::options::EnvironmentConfig::default(),
                        ),
                    );
                    if let Ok(lr) = lower_result {
                        eprintln!("HIR blocks:");
                        for (block_id, block) in &lr.func.body.blocks {
                            eprintln!("  Block {:?}:", block_id);
                            for instr in &block.instructions {
                                eprintln!("    {:?} = {:?}", instr.lvalue.identifier, &instr.value);
                            }
                            eprintln!("    terminal: {:?}", block.terminal);
                        }
                    }
                }
            }
        }
        return;
    }
}
