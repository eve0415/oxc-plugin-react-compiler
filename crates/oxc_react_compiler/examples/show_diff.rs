fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let targets = [
        "alias-capture-in-method-receiver-and-mutate",
        "allow-ref-type-cast-in-render",
        "array-expression-spread",
        "destructure-string-literal-invalid-identifier-property-key",
        "destructure-string-literal-property-key",
    ];

    for target in targets {
        let path = format!("{}/{}.js", fixture_dir, target);
        let expect_path = format!("{}/{}.expect.md", fixture_dir, target);
        if !std::path::Path::new(&path).exists() {
            // Try .ts, .tsx, .jsx
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let expected_code = extract_code_block(&expect_md).unwrap();

        let filename = format!("{}.js", target);
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        eprintln!("=== {} ===", target);
        eprintln!("--- ACTUAL ---");
        for (i, line) in result.code.lines().enumerate() {
            eprintln!("{:3}: {}", i + 1, line);
        }
        eprintln!("--- EXPECTED ---");
        for (i, line) in expected_code.lines().enumerate() {
            eprintln!("{:3}: {}", i + 1, line);
        }
        eprintln!();
    }
}

fn extract_code_block(md: &str) -> Option<String> {
    let code_header_idx = md.find("## Code")?;
    let rest = &md[code_header_idx..];
    let block_start = rest.find("```")?;
    let after_start = &rest[block_start + 3..];
    let newline = after_start.find('\n')?;
    let code_start = &after_start[newline + 1..];
    let block_end = code_start.find("```")?;
    Some(code_start[..block_end].to_string())
}
