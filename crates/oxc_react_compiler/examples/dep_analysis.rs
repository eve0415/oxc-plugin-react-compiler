fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut dep_mismatch = 0;
    let mut dep_mismatch_only = 0; // fixtures where dep is the ONLY difference

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

        // Check if the only differences are dep-related lines
        let max = actual_lines.len().max(expected_lines.len());
        let mut diff_lines = Vec::new();
        let mut all_diffs_are_deps = true;

        for i in 0..max {
            let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
            let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
            if a != e {
                diff_lines.push((a.to_string(), e.to_string()));
                // Check if this is a dep-related diff
                let is_dep = (a.contains("$[")
                    && a.contains(" !== ")
                    && e.contains("$[")
                    && e.contains(" !== "))
                    || (a.contains("$[")
                        && a.contains(" = ")
                        && e.contains("$[")
                        && e.contains(" = "))
                    || (a.contains("_c(") && e.contains("_c("));
                if !is_dep {
                    all_diffs_are_deps = false;
                }
            }
        }

        let has_dep_mismatch = diff_lines.iter().any(|(a, e)| {
            (a.contains("$[") && a.contains(" !== ") && e.contains("$[") && e.contains(" !== "))
        });

        if has_dep_mismatch {
            dep_mismatch += 1;
            if all_diffs_are_deps {
                dep_mismatch_only += 1;
                if dep_mismatch_only <= 20 {
                    eprintln!("--- {}", stem);
                    for (a, e) in &diff_lines {
                        eprintln!("  A: {}", a);
                        eprintln!("  E: {}", e);
                    }
                }
            }
        }
    }

    eprintln!("\nFixtures with dep tracking mismatch: {}", dep_mismatch);
    eprintln!(
        "Fixtures where dep is ONLY difference: {}",
        dep_mismatch_only
    );
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
