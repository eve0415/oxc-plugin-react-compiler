fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut near_passes: Vec<(String, String, String, bool)> = Vec::new();

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

        // Use the REAL normalize from conformance
        let actual = normalize_code(&result.code);
        let expected = normalize_code(&expected_code);
        if actual == expected {
            continue;
        }

        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();

        let max_lines = actual_lines.len().max(expected_lines.len());
        let mut diff_count = 0;
        let mut first_diff_a = String::new();
        let mut first_diff_e = String::new();
        for i in 0..max_lines {
            let a = actual_lines.get(i).unwrap_or(&"<MISSING>").trim();
            let e = expected_lines.get(i).unwrap_or(&"<MISSING>").trim();
            if a != e {
                if diff_count == 0 {
                    first_diff_a = a.to_string();
                    first_diff_e = e.to_string();
                }
                diff_count += 1;
            }
        }

        if diff_count <= 3 {
            near_passes.push((stem, first_diff_a, first_diff_e, diff_count <= 1));
        }
    }

    near_passes.sort_by_key(|x| x.0.clone());
    eprintln!(
        "Near-passing fixtures (<=3 diff lines, real normalize): {}",
        near_passes.len()
    );
    for (stem, a, e, is_one) in &near_passes {
        let marker = if *is_one { "***" } else { "   " };
        eprintln!("{} {}", marker, stem);
        eprintln!("     A: {}", a);
        eprintln!("     E: {}", e);
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
            normalize_import_line(trimmed)
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_import_line(line: &str) -> String {
    let mut s = line.to_string();
    s = normalize_quotes(&s);
    s = normalize_destructuring(&s);
    s
}

fn normalize_quotes(line: &str) -> String {
    line.replace('\'', "\"")
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
