fn main() {
    // Simple test: arrow function that should have a body
    let source = r#"
function Component(props) {
    const fn1 = (x) => x + 1;
    return fn1(props.a);
}
"#;
    let options = oxc_react_compiler::options::PluginOptions::default();
    let result = oxc_react_compiler::compile("test.js", source, &options);
    eprintln!("Transformed: {}", result.transformed);
    eprintln!("Code:\n{}", result.code);
}
