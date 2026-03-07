use std::collections::HashMap;

fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut patterns: HashMap<String, usize> = HashMap::new();
    let mut diff_sizes: Vec<(String, usize)> = Vec::new();

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
        if !result.code.contains("_c(") {
            continue;
        }

        let actual = normalize_code(&result.code);
        let expected = normalize_code(&expected_code);
        if actual == expected {
            continue;
        }

        let a_size = extract_cache_size(&actual);
        let e_size = extract_cache_size(&expected);
        if a_size != e_size {
            continue;
        }

        // Same cache size, different body
        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();
        let diff = line_diff(&a_lines, &e_lines);
        diff_sizes.push((stem.clone(), diff));

        // Find first differing line
        let min_len = a_lines.len().min(e_lines.len());
        for i in 0..min_len {
            if a_lines[i] != e_lines[i] {
                // Categorize the diff
                let a_line = a_lines[i];
                let e_line = e_lines[i];

                if e_line.contains(" ? ") && !a_line.contains(" ? ") {
                    *patterns.entry("missing_ternary".to_string()).or_insert(0) += 1;
                } else if e_line.contains(" && ")
                    || e_line.contains(" || ")
                    || e_line.contains(" ?? ")
                {
                    if !a_line.contains(" && ")
                        && !a_line.contains(" || ")
                        && !a_line.contains(" ?? ")
                    {
                        *patterns.entry("missing_logical".to_string()).or_insert(0) += 1;
                    } else {
                        *patterns.entry("diff_logical".to_string()).or_insert(0) += 1;
                    }
                } else if e_line.contains("const ") && a_line.contains("const ") {
                    *patterns.entry("diff_declaration".to_string()).or_insert(0) += 1;
                } else if e_line.starts_with("if (") || a_line.starts_with("if (") {
                    *patterns.entry("diff_condition".to_string()).or_insert(0) += 1;
                } else if e_line.contains("return ") || a_line.contains("return ") {
                    *patterns.entry("diff_return".to_string()).or_insert(0) += 1;
                } else {
                    *patterns.entry("other".to_string()).or_insert(0) += 1;
                }
                break;
            }
        }
        if a_lines.len() != e_lines.len() {
            *patterns.entry("diff_line_count".to_string()).or_insert(0) += 1;
        }
    }

    diff_sizes.sort_by_key(|x| x.1);

    eprintln!("=== Body Diff Analysis (same cache size) ===");
    eprintln!("Total: {}", diff_sizes.len());
    eprintln!("\nFirst diff patterns:");
    let mut sorted: Vec<_> = patterns.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    for (pattern, count) in &sorted {
        eprintln!("  {}: {}", pattern, count);
    }

    eprintln!("\nSmallest diffs (diff <= 5):");
    for (name, diff) in diff_sizes.iter().take(30) {
        if *diff > 5 {
            break;
        }
        eprintln!("  diff={}: {}", diff, name);
    }
}

fn line_diff(a: &[&str], e: &[&str]) -> usize {
    let max_len = a.len().max(e.len());
    let min_len = a.len().min(e.len());
    let mut diff = max_len - min_len;
    for i in 0..min_len {
        if a[i] != e[i] {
            diff += 1;
        }
    }
    diff
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
    let lines_normalized: String = code
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            let mut s = trimmed.to_string();
            s = s.replace('\'', "\"");
            s = normalize_destructuring(&s);
            s
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    normalize_block_arrows(&lines_normalized)
}

fn normalize_block_arrows(code: &str) -> String {
    let mut result = code.to_string();
    loop {
        let search = "=> {\nreturn ";
        let Some(start) = result.find(search) else {
            break;
        };
        let after = &result[start + search.len()..];
        if let Some(semi_pos) = after.find(";\n}") {
            let expr = &after[..semi_pos];
            if !expr.contains('\n') {
                let end = start + search.len() + semi_pos + 3;
                let replacement = format!("=> {}", expr);
                result = format!("{}{}{}", &result[..start], replacement, &result[end..]);
                continue;
            }
        }
        break;
    }
    result
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
