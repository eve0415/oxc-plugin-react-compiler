fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut patterns: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut examples: std::collections::HashMap<String, Vec<String>> =
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
        if !result.transformed {
            continue;
        }

        let actual = normalize_code(&result.code);
        let expected = normalize_code(&expected_code);
        if actual == expected {
            continue;
        }

        // Only look at single-scope with same cache size
        let a_scopes = actual
            .matches("Symbol.for(\"react.memo_cache_sentinel\")")
            .count();
        let e_scopes = expected
            .matches("Symbol.for(\"react.memo_cache_sentinel\")")
            .count();
        if a_scopes != 1 || e_scopes != 1 {
            continue;
        }

        let a_size = extract_cache_size(&actual);
        let e_size = extract_cache_size(&expected);
        if a_size != e_size {
            continue;
        }

        // Categorize the diff
        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();

        let mut diff_categories = Vec::new();
        for (a, e) in a_lines.iter().zip(e_lines.iter()) {
            if a == e {
                continue;
            }
            if a.contains("let t") && !e.contains("let t") {
                diff_categories.push("temp_vs_named_let".to_string());
            } else if a.contains("return t") && !e.contains("return t") {
                diff_categories.push("return_temp_vs_named".to_string());
            } else if a.contains("$[0] = t") && !e.contains("$[0] = t") {
                diff_categories.push("cache_store_temp".to_string());
            } else if a.contains("t0 = $[0]") && !e.contains("t0 = $[0]") {
                diff_categories.push("cache_load_temp".to_string());
            } else if a.starts_with("const ") != e.starts_with("const ") {
                diff_categories.push("const_mismatch".to_string());
            } else if a.contains("() => {") && a.trim().ends_with("}") && !e.contains("() => {") {
                diff_categories.push("empty_arrow_body".to_string());
            } else if a.trim() == "}" && e.trim() != "}" {
                diff_categories.push("missing_body".to_string());
            } else {
                diff_categories.push("other".to_string());
            }
        }
        if a_lines.len() != e_lines.len() {
            diff_categories.push(format!("line_count_{}vs{}", a_lines.len(), e_lines.len()));
        }

        diff_categories.sort();
        diff_categories.dedup();
        let cat = diff_categories.join("+");
        *patterns.entry(cat.clone()).or_default() += 1;
        examples.entry(cat).or_default().push(stem);
    }

    let mut sorted: Vec<_> = patterns.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (cat, count) in sorted.iter().take(25) {
        let exs = &examples[cat];
        let show: Vec<_> = exs.iter().take(3).cloned().collect();
        eprintln!("{:>4} {} (e.g. {})", count, cat, show.join(", "));
    }
}

fn extract_cache_size(code: &str) -> Option<u32> {
    for line in code.lines() {
        if let Some(pos) = line.find("_c(") {
            let rest = &line[pos + 3..];
            if let Some(end) = rest.find(')') {
                return rest[..end].parse().ok();
            }
        }
    }
    None
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
                if !inner_trimmed.contains('{') && !inner_trimmed.contains('}') {
                    result.push_str(&format!("{{ {} }}", inner_trimmed));
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
