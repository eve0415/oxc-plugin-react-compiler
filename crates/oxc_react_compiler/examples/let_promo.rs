fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let targets = vec![
        "capture-indirect-mutate-alias-iife",
        "capturing-fun-alias-captured-mutate-arr-2-iife",
        "for-of-simple",
        "while-simple",
    ];

    for target in targets {
        // Try .js, .jsx, .ts, .tsx
        let mut found = false;
        for ext in &["js", "jsx", "ts", "tsx"] {
            let path = format!("{}/{}.{}", fixture_dir, target, ext);
            if std::path::Path::new(&path).exists() {
                let source = std::fs::read_to_string(&path).unwrap();
                let expect_path = format!("{}/{}.expect.md", fixture_dir, target);
                let expect_md = std::fs::read_to_string(&expect_path).unwrap();
                let expected_code = extract_code_block(&expect_md).unwrap();

                let filename = format!("{}.{}", target, ext);
                let result = oxc_react_compiler::compile(&filename, &source, &options);

                eprintln!("=== {} ===", target);
                eprintln!("--- Source ---");
                for line in source.lines().take(15) {
                    eprintln!("  {}", line);
                }
                eprintln!("--- Actual (first 20 lines) ---");
                for (i, line) in result.code.lines().enumerate().take(20) {
                    eprintln!("{:3}: {}", i + 1, line);
                }
                eprintln!("--- Expected (first 20 lines) ---");
                for (i, line) in expected_code.lines().enumerate().take(20) {
                    eprintln!("{:3}: {}", i + 1, line);
                }
                eprintln!();
                found = true;
                break;
            }
        }
        if !found {
            eprintln!("NOT FOUND: {}", target);
        }
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
