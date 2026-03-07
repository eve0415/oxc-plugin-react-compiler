fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut diff_patterns: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let mut examples: std::collections::HashMap<String, String> = std::collections::HashMap::new();

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
        if !expected_code.contains("_c(") {
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

        let a_scopes = actual
            .matches("Symbol.for(\"react.memo_cache_sentinel\")")
            .count();
        let e_scopes = expected
            .matches("Symbol.for(\"react.memo_cache_sentinel\")")
            .count();
        if a_scopes != 0 || e_scopes != 0 {
            continue;
        }

        // Categorize the diff
        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();
        let diff_count = compute_diff_count(&a_lines, &e_lines);

        let pattern = if diff_count <= 2 {
            "tiny_diff".to_string()
        } else if diff_count <= 5 {
            "small_diff".to_string()
        } else if diff_count <= 10 {
            "medium_diff".to_string()
        } else {
            "large_diff".to_string()
        };

        *diff_patterns.entry(pattern.clone()).or_default() += 1;
        if !examples.contains_key(&pattern) || diff_count <= 3 {
            examples.insert(pattern, stem);
        }
    }

    let mut sorted: Vec<_> = diff_patterns.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (cat, count) in &sorted {
        eprintln!("{:>4} {} (e.g. {})", count, cat, examples[cat]);
    }
}

fn compute_diff_count(a: &[&str], e: &[&str]) -> usize {
    let mut diff = 0;
    for i in 0..a.len().max(e.len()) {
        if i >= a.len() || i >= e.len() || a[i] != e[i] {
            diff += 1;
        }
    }
    diff
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
