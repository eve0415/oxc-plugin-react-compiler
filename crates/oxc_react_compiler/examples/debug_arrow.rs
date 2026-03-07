fn main() {
    let source = r#"const fn1 = (x) => x + 1;"#;
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::mjs();
    let parser_ret = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    for stmt in &parser_ret.program.body {
        if let oxc_ast::ast::Statement::VariableDeclaration(vd) = stmt {
            for d in &vd.declarations {
                if let Some(oxc_ast::ast::Expression::ArrowFunctionExpression(arrow)) = &d.init {
                    eprintln!("expression: {}", arrow.expression);
                    eprintln!("body.statements.len: {}", arrow.body.statements.len());
                    for s in &arrow.body.statements {
                        eprintln!("  statement: {:?}", std::mem::discriminant(s));
                        match s {
                            oxc_ast::ast::Statement::ExpressionStatement(expr) => {
                                eprintln!(
                                    "    expr kind: {:?}",
                                    std::mem::discriminant(&expr.expression)
                                );
                            }
                            oxc_ast::ast::Statement::ReturnStatement(_) => {
                                eprintln!("    return");
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
