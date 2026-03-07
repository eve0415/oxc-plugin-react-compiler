fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

    let path = format!("{}/for-of-simple.js", fixture_dir);
    let source = std::fs::read_to_string(&path).unwrap();

    eprintln!("Source:");
    for line in source.lines() {
        eprintln!("  {}", line);
    }

    let result = oxc_react_compiler::compile("for-of-simple.js", &source, &options);
    eprintln!("\nOutput:");
    for line in result.code.lines() {
        eprintln!("  {}", line);
    }
}
