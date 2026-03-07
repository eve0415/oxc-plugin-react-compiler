fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let mut options = oxc_react_compiler::options::PluginOptions::default();
    options.compilation_mode = oxc_react_compiler::options::CompilationMode::All;
    options.panic_threshold = oxc_react_compiler::options::PanicThreshold::All;
    let args: Vec<String> = std::env::args().collect();
    let filter = args.get(1).map(|s| s.as_str()).unwrap_or("simple-scope");

    for entry in std::fs::read_dir(fixture_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        if stem != filter {
            continue;
        }

        let source = std::fs::read_to_string(&path).unwrap();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        eprintln!("=== ACTUAL OUTPUT ===");
        for (i, line) in result.code.lines().enumerate() {
            eprintln!("{:3}: {}", i + 1, line);
        }

        let expect_path = std::path::Path::new(fixture_dir).join(format!("{stem}.expect.md"));
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        if let Some(expected) = extract_code_block(&expect_md) {
            eprintln!("\n=== EXPECTED OUTPUT ===");
            for (i, line) in expected.lines().enumerate() {
                eprintln!("{:3}: {}", i + 1, line);
            }
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
