fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut issues: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

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

        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();

        // Check expected for multi-scope indicators
        let scope_count = expected_lines
            .iter()
            .filter(|l| {
                l.contains("Symbol.for(\"react.memo_cache_sentinel\")")
                    || (l.contains("$[") && l.contains(" !== "))
            })
            .count();
        let has_multi_scope = scope_count > 1;

        // Check for _c(N) mismatch
        let actual_c = actual_lines
            .iter()
            .find(|l| l.contains("_c("))
            .map(|l| l.trim().to_string());
        let expected_c = expected_lines
            .iter()
            .find(|l| l.contains("_c("))
            .map(|l| l.trim().to_string());
        let cache_mismatch = actual_c != expected_c;

        // Categorize
        if has_multi_scope {
            *issues.entry("multi_scope".to_string()).or_insert(0) += 1;
        } else if cache_mismatch {
            *issues.entry("cache_size_wrong".to_string()).or_insert(0) += 1;
        } else {
            // Single scope, correct cache size — what's the diff?
            let max = actual_lines.len().max(expected_lines.len());
            let mut first_diff_e = String::new();
            for i in 0..max {
                let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
                let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
                if a != e {
                    first_diff_e = e.to_string();
                    break;
                }
            }

            let cat = if first_diff_e.contains("useMemo") || first_diff_e.contains("useCallback") {
                "manual_memo"
            } else if first_diff_e.contains("$[") && first_diff_e.contains(" !== ") {
                "dep_tracking"
            } else if first_diff_e.contains("$[") && first_diff_e.contains(" = ") {
                "cache_store"
            } else if first_diff_e.starts_with("const ") {
                "const_decl"
            } else if first_diff_e.starts_with("let ") {
                "let_decl"
            } else if first_diff_e.starts_with("return ") {
                "return_diff"
            } else if first_diff_e.starts_with("if (") || first_diff_e.starts_with("} else") {
                "control_flow"
            } else if first_diff_e.contains("function") {
                "function_diff"
            } else if first_diff_e == "<MISSING>" {
                "extra_actual_lines"
            } else {
                "other_single_scope"
            };
            *issues.entry(cat.to_string()).or_insert(0) += 1;
        }
    }

    let mut sorted: Vec<_> = issues.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    for (cat, count) in &sorted {
        eprintln!("{:4} {}", count, cat);
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
