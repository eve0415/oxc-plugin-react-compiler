fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut patterns: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut examples: std::collections::HashMap<String, Vec<(String, String, String)>> =
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

        let al: Vec<&str> = actual.lines().collect();
        let el: Vec<&str> = expected.lines().collect();

        // Check single scope + correct cache
        let scope_count = el
            .iter()
            .filter(|l| {
                l.contains("Symbol.for(\"react.memo_cache_sentinel\")")
                    || (l.contains("$[") && l.contains(" !== "))
            })
            .count();
        if scope_count > 1 {
            continue;
        }
        let ac = al
            .iter()
            .find(|l| l.contains("_c("))
            .map(|l| l.trim().to_string());
        let ec = el
            .iter()
            .find(|l| l.contains("_c("))
            .map(|l| l.trim().to_string());
        if ac != ec {
            continue;
        }

        let max = al.len().max(el.len());
        for i in 0..max {
            let a = al.get(i).unwrap_or(&"<MISSING>").trim();
            let e = el.get(i).unwrap_or(&"<MISSING>").trim();
            if a != e {
                let pat = if e.starts_with("const ") && a.starts_with("const ") {
                    // Sub-categorize const differences
                    if e.contains(" = [") || e.contains(" = {") {
                        "const_literal_assign"
                    } else if e.contains(" = <") {
                        "const_jsx_assign"
                    } else if e.contains("(") {
                        "const_call_assign"
                    } else {
                        "const_other"
                    }
                } else if e.starts_with("let ") && a.starts_with("let ") {
                    "let_decl"
                } else if (e.starts_with("const ") && !a.starts_with("const "))
                    || (!e.starts_with("const ") && a.starts_with("const "))
                {
                    "const_vs_non_const"
                } else if (e.starts_with("let ") && !a.starts_with("let "))
                    || (!e.starts_with("let ") && a.starts_with("let "))
                {
                    "let_vs_non_let"
                } else {
                    "other"
                };
                *patterns.entry(pat.to_string()).or_insert(0) += 1;
                examples.entry(pat.to_string()).or_default().push((
                    stem.clone(),
                    a.to_string(),
                    e.to_string(),
                ));
                break;
            }
        }
    }

    let mut sorted: Vec<_> = patterns.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (pat, count) in &sorted {
        eprintln!("{:4} {}", count, pat);
        if let Some(exs) = examples.get(pat.as_str()) {
            for (stem, a, e) in exs.iter().take(3) {
                eprintln!("     {} A: {}", stem, a);
                eprintln!("     {} E: {}", stem, e);
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
