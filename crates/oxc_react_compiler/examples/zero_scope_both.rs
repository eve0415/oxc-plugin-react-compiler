fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut categories: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

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

        // Both have 0 scopes but different outputs
        if a_scopes != 0 || e_scopes != 0 {
            continue;
        }

        // Check if the function body differs - look at what changed
        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();

        // Find first differing line
        let first_diff = a_lines.iter().zip(e_lines.iter()).position(|(a, e)| a != e);

        let cat = if actual.contains("_c(") != expected.contains("_c(") {
            "cache_diff".to_string()
        } else if a_lines.len() != e_lines.len() {
            let diff = (a_lines.len() as i64 - e_lines.len() as i64);
            if diff.abs() <= 3 {
                format!("lines_off_by_{}", diff)
            } else {
                format!("lines_off_by_many({})", diff)
            }
        } else if let Some(idx) = first_diff {
            let a_line = a_lines[idx];
            let e_line = e_lines[idx];
            if a_line.contains("_c(") || e_line.contains("_c(") {
                "cache_line_diff".to_string()
            } else if a_line.contains("const") != e_line.contains("const") {
                "const_keyword_diff".to_string()
            } else if a_line.contains("let") != e_line.contains("let") {
                "let_keyword_diff".to_string()
            } else {
                "content_diff".to_string()
            }
        } else {
            "unknown".to_string()
        };

        *categories.entry(cat).or_default() += 1;
    }

    let mut sorted: Vec<_> = categories.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (cat, count) in &sorted {
        eprintln!("{:>4} {}", count, cat);
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
