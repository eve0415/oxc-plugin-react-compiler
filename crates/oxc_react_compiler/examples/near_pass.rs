fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let args: Vec<String> = std::env::args().collect();
    let max_diffs = args
        .iter()
        .position(|a| a == "--max-diffs")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(4);

    let mut near_misses: Vec<(String, usize, String)> = Vec::new();
    let mut total = 0;
    let mut already_passing = 0;
    let mut not_transformed = 0;
    let mut skipped = 0;

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
            skipped += 1;
            continue;
        }

        let source = std::fs::read_to_string(&path).unwrap();
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let Some(expected_code) = extract_code_block(&expect_md) else {
            skipped += 1;
            continue;
        };

        if !expected_code.contains("_c(") {
            skipped += 1;
            continue;
        }

        total += 1;

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        let actual = normalize(&result.code);
        let expected = normalize(&expected_code);

        if actual == expected {
            already_passing += 1;
            continue;
        }

        if !result.transformed {
            not_transformed += 1;
            continue;
        }

        // Count differing lines
        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();
        let max_lines = actual_lines.len().max(expected_lines.len());
        let mut diff_count = 0;
        let mut first_diff = String::new();
        for i in 0..max_lines {
            let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
            let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
            if a != e {
                if diff_count == 0 {
                    first_diff = format!(
                        "actual='{}' expected='{}'",
                        a.chars().take(50).collect::<String>(),
                        e.chars().take(50).collect::<String>()
                    );
                }
                diff_count += 1;
            }
        }

        if diff_count <= max_diffs {
            near_misses.push((stem, diff_count, first_diff));
        }
    }

    near_misses.sort_by_key(|x| x.1);

    eprintln!("Total fixtures with _c(): {}", total);
    eprintln!("Already passing: {}", already_passing);
    eprintln!("Not transformed: {}", not_transformed);
    eprintln!("Skipped: {}", skipped);
    eprintln!(
        "\nNear misses ({} fixtures with <= {} line diffs):",
        near_misses.len(),
        max_diffs
    );
    for (name, diff_count, first_diff) in &near_misses {
        eprintln!("  {:2} diffs: {} — {}", diff_count, name, first_diff);
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
            normalize_import_line(trimmed)
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_import_line(line: &str) -> String {
    let mut s = line.to_string();
    s = s.replace('\'', "\"");
    s = normalize_destructuring(&s);
    s
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
