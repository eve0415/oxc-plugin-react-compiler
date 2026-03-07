fn main() {
    let source = r#"
function Component(props) {
    const fn1 = (x) => x + 1;
    return fn1(props.a);
}
"#;
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::jsx();
    let parser_ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parser_ret.program)
        .semantic;

    for stmt in &parser_ret.program.body {
        if let oxc_ast::ast::Statement::FunctionDeclaration(func) = stmt {
            let name = func.id.as_ref().map(|id| id.name.as_str());
            if let Some(body) = func.body.as_ref() {
                let lower_result = oxc_react_compiler::hir::build::lower_function(
                    body,
                    &func.params,
                    oxc_react_compiler::hir::build::LoweringContext::new(
                        &semantic,
                        source,
                        oxc_react_compiler::environment::Environment::new(
                            oxc_react_compiler::options::EnvironmentConfig::default(),
                        ),
                    ),
                    oxc_react_compiler::hir::build::LowerFunctionOptions::function(
                        name,
                        func.span,
                        func.generator,
                        func.r#async,
                    ),
                );
                if let Ok(lr) = lower_result {
                    for (_block_id, block) in &lr.func.body.blocks {
                        for instr in &block.instructions {
                            if let oxc_react_compiler::hir::types::InstructionValue::FunctionExpression { lowered_func, .. } = &instr.value {
                                eprintln!("=== Inner function HIR ===");
                                for (bid, b) in &lowered_func.func.body.blocks {
                                    eprintln!("  Block {:?}:", bid);
                                    for inner_instr in &b.instructions {
                                        let lv_name = match &inner_instr.lvalue.identifier.name {
                                            Some(n) => format!("{:?}", n),
                                            None => format!("_t{}", inner_instr.lvalue.identifier.id.0),
                                        };
                                        eprintln!("    {} = {:?}", lv_name, std::mem::discriminant(&inner_instr.value));
                                        // Print brief info about the instruction
                                        match &inner_instr.value {
                                            oxc_react_compiler::hir::types::InstructionValue::LoadLocal { place, .. } => {
                                                eprintln!("      LoadLocal({:?})", place.identifier.name);
                                            }
                                            oxc_react_compiler::hir::types::InstructionValue::BinaryExpression { operator, .. } => {
                                                eprintln!("      BinaryExpression({:?})", operator);
                                            }
                                            oxc_react_compiler::hir::types::InstructionValue::Primitive { value, .. } => {
                                                eprintln!("      Primitive({:?})", value);
                                            }
                                            _ => {}
                                        }
                                    }
                                    eprintln!("    terminal: {:?}", std::mem::discriminant(&b.terminal));
                                    if let oxc_react_compiler::hir::types::Terminal::Return { value, return_variant, .. } = &b.terminal {
                                        eprintln!("      Return value: {:?}, variant: {:?}", value.identifier, return_variant);
                                    }
                                }
                                eprintln!("  Params:");
                                for p in &lowered_func.func.params {
                                    if let oxc_react_compiler::hir::types::Argument::Place(place) = p {
                                        eprintln!("    {:?}", place.identifier);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
