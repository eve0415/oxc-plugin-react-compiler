fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut no_change_expected = 0;
    let mut has_hooks_expected = 0;
    let mut has_jsx_expected = 0;
    let mut empty_func_expected = 0;
    let mut samples: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(fixture_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        match ext {
            Some("js" | "jsx" | "ts" | "tsx") => {}
            _ => continue,
        }

        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let expect_path = std::path::Path::new(fixture_dir).join(format!("{stem}.expect.md"));
        if !expect_path.exists() {
            continue;
        }

        let source = std::fs::read_to_string(&path).unwrap();
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let Some(expected_code) = extract_code_block(&expect_md) else {
            continue;
        };

        // We're looking for: expected doesn't have _c( but we produce it
        if expected_code.contains("_c(") {
            continue;
        }

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);
        if !result.transformed {
            continue;
        }

        let actual = normalize_code(&result.code);
        let expected = normalize_code(&expected_code);
        if actual == expected {
            continue;
        }

        // This is a false memo case - we memoized but shouldn't have

        // Check if expected output == source (no changes at all)
        let source_norm = normalize_code(&source);
        if expected == source_norm {
            no_change_expected += 1;
        }

        // Check patterns in expected
        if expected_code.contains("useState")
            || expected_code.contains("useEffect")
            || expected_code.contains("useRef")
        {
            has_hooks_expected += 1;
        }
        if expected_code.contains("<") && expected_code.contains("/>") {
            has_jsx_expected += 1;
        }

        if samples.len() < 10 {
            samples.push(stem.clone());
        }
    }

    eprintln!("=== False Memo Analysis ===");
    eprintln!(
        "no_change_expected (output == input): {}",
        no_change_expected
    );
    eprintln!("has_hooks: {}", has_hooks_expected);
    eprintln!("has_jsx: {}", has_jsx_expected);
    eprintln!("\nSample fixtures:");
    for s in &samples {
        eprintln!("  {}", s);
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

fn normalize_code(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            let mut s = trimmed.to_string();
            s = s.replace('\'', "\"");
            s
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
