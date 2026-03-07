fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: show_fixture <fixture_stem>");
        return;
    }
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let mut options = oxc_react_compiler::options::PluginOptions::default();
    options.compilation_mode = oxc_react_compiler::options::CompilationMode::All;
    let stem = &args[1];

    let fixture_path = std::path::Path::new(fixture_dir);
    // Find the fixture by stem
    for entry in std::fs::read_dir(fixture_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let file_stem = path.file_stem().unwrap().to_string_lossy().to_string();
        if file_stem != *stem {
            continue;
        }

        let expect_path = fixture_path.join(format!("{stem}.expect.md"));
        let source = std::fs::read_to_string(&path).unwrap();
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let expected_code = extract_code_block(&expect_md).unwrap_or_default();

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        eprintln!("=== SOURCE ===");
        eprintln!("{}", source);
        eprintln!("\n=== EXPECTED ===");
        eprintln!("{}", expected_code);
        eprintln!("\n=== ACTUAL ===");
        eprintln!("{}", result.code);
        eprintln!("\n=== NORMALIZED EXPECTED ===");
        eprintln!("{}", normalize_code(&expected_code));
        eprintln!("\n=== NORMALIZED ACTUAL ===");
        eprintln!("{}", normalize_code(&result.code));
        return;
    }
    eprintln!("Fixture '{}' not found", stem);
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

fn normalize_code(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            let mut s = trimmed.to_string();
            s = s.replace('\'', "\"");
            s = normalize_destructuring(&s);
            s
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_destructuring(line: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < chars.len() && depth > 0 {
                if chars[j] == '{' {
                    depth += 1;
                }
                if chars[j] == '}' {
                    depth -= 1;
                }
                j += 1;
            }
            if depth == 0 {
                let inner: String = chars[i + 1..j - 1].iter().collect();
                let inner_trimmed = inner.trim();
                let inner_normalized = normalize_destructuring(inner_trimmed);
                result.push_str(&format!("{{ {} }}", inner_normalized));
                i = j;
                continue;
            }
            result.push(chars[i]);
            i += 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}
