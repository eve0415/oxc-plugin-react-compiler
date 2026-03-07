fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

    let mut categories: std::collections::HashMap<String, Vec<(String, usize)>> =
        std::collections::HashMap::new();

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

        let actual = normalize(&result.code);
        let expected = normalize(&expected_code);

        if actual == expected {
            continue;
        }
        if !result.transformed {
            continue;
        }

        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();
        let max_lines = actual_lines.len().max(expected_lines.len());
        let mut diff_count = 0;
        let mut first_diff_expected = String::new();
        for i in 0..max_lines {
            let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
            let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
            if a != e {
                if diff_count == 0 {
                    first_diff_expected = e.to_string();
                }
                diff_count += 1;
            }
        }

        // Categorize by first diff pattern
        let cat = if first_diff_expected.contains("_temp") {
            "function_outlining".to_string()
        } else if first_diff_expected.contains("_c(")
            && !first_diff_expected.contains("_c(1)")
            && !first_diff_expected.contains("_c(2)")
        {
            format!(
                "wrong_cache_size: {}",
                first_diff_expected.chars().take(60).collect::<String>()
            )
        } else if first_diff_expected.contains("if (")
            || first_diff_expected.contains("} else")
            || first_diff_expected.contains("for (")
            || first_diff_expected.contains("while (")
            || first_diff_expected.contains("switch (")
        {
            "control_flow".to_string()
        } else if first_diff_expected.contains("useRef")
            || first_diff_expected.contains("useState")
            || first_diff_expected.contains("useCallback")
            || first_diff_expected.contains("useEffect")
            || first_diff_expected.contains("useMemo")
        {
            "hooks".to_string()
        } else if first_diff_expected.starts_with("import ")
            || (first_diff_expected.starts_with("const {") && first_diff_expected.contains("from"))
        {
            "import_mismatch".to_string()
        } else if first_diff_expected.starts_with("let ")
            || first_diff_expected.starts_with("const ")
        {
            "variable_declaration".to_string()
        } else if first_diff_expected.contains("return ") {
            "return_mismatch".to_string()
        } else {
            format!(
                "other: {}",
                first_diff_expected.chars().take(60).collect::<String>()
            )
        };

        categories.entry(cat).or_default().push((stem, diff_count));
    }

    let mut sorted: Vec<_> = categories.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));

    for (cat, fixtures) in &sorted {
        eprintln!("{:3} {}", fixtures.len(), cat);
        for (name, diffs) in fixtures.iter().take(5) {
            eprintln!("      ({} diffs) {}", diffs, name);
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

fn normalize(code: &str) -> String {
    code.lines()
        .map(|line| {
            let trimmed = line.trim();
            let mut s = trimmed.to_string();
            s = s.replace('\'', "\"");
            normalize_destructuring(&s)
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_destructuring(line: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let start = i;
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
                let inner: String = chars[start + 1..j - 1].iter().collect();
                let inner = inner.trim();
                if !inner.contains('{') && !inner.contains('}') {
                    result.push_str(&format!("{{ {} }}", inner));
                    i = j;
                    continue;
                }
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
