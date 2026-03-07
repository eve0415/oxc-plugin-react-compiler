fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut diff_1: Vec<(String, usize)> = Vec::new();
    let mut diff_2: Vec<(String, usize)> = Vec::new();

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

        let a_size = extract_cache_size(&actual);
        let e_size = extract_cache_size(&expected);

        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();
        let diff = line_diff(&a_lines, &e_lines);

        if a_size == e_size {
            if a_size == Some(1) {
                diff_1.push((stem, diff));
            } else if a_size == Some(2) {
                diff_2.push((stem, diff));
            }
        }
    }

    diff_1.sort_by_key(|x| x.1);
    diff_2.sort_by_key(|x| x.1);

    eprintln!("=== Same cache size 1 (smallest diffs) ===");
    for (name, diff) in diff_1.iter().take(15) {
        eprintln!("  diff={}: {}", diff, name);
    }
    eprintln!("\n=== Same cache size 2 (smallest diffs) ===");
    for (name, diff) in diff_2.iter().take(15) {
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
