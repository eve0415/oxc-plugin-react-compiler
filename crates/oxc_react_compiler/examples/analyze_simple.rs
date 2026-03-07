/// Analyze _c(1) "no_post_scope" fixtures (the simplest pattern) that are failing.
fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

    let mut issues: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut total = 0;
    let mut passing = 0;

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

        if !expected_code.contains("_c(1)") || expected_code.contains("_c(2)") {
            continue;
        }

        // Check if this is a "no_post_scope" fixture
        let exp_lines: Vec<&str> = expected_code
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let else_line = exp_lines.iter().position(|l| *l == "} else {");
        let Some(else_idx) = else_line else {
            continue;
        };
        let after_else = exp_lines.get(else_idx + 2);
        if after_else != Some(&"}") {
            continue;
        }
        let close_else_idx = else_idx + 2;
        let return_line = exp_lines.iter().position(|l| l.starts_with("return "));
        let Some(return_idx) = return_line else {
            continue;
        };
        if close_else_idx + 1 > return_idx {
            continue;
        }
        let post_scope_lines: Vec<&&str> =
            exp_lines[close_else_idx + 1..return_idx].iter().collect();
        if !post_scope_lines.is_empty() {
            continue;
        }

        total += 1;

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        let actual = normalize(&result.code);
        let expected = normalize(&expected_code);

        if actual == expected {
            passing += 1;
            continue;
        }

        // Categorize the issue
        if !result.transformed {
            issues
                .entry("not_transformed".to_string())
                .or_default()
                .push(stem);
            continue;
        }

        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();

        let mut issue = "unknown".to_string();
        for i in 0..actual_lines.len().max(expected_lines.len()) {
            let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
            let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
            if a == e {
                continue;
            }

            if e.contains("_temp") {
                issue = "function_outlining".to_string();
            } else if e.contains("\"use ") || e.contains("'use ") {
                issue = "directive".to_string();
            } else if a.starts_with("let ") && e.starts_with("let ") && a != e {
                issue = format!("wrong_let: actual='{}' expected='{}'", a, e);
            } else if e.starts_with("import ") || (e.starts_with("const {") && e.contains("from")) {
                issue = "import_mismatch".to_string();
            } else if e.contains("if (")
                || e.contains("} else")
                || e.contains("for (")
                || e.contains("switch (")
            {
                issue = "control_flow".to_string();
            } else if e.contains(".map(")
                || e.contains(".push(")
                || e.contains(".forEach(")
                || e.contains(".filter(")
            {
                issue = "array_method".to_string();
            } else if e.contains("useRef")
                || e.contains("useState")
                || e.contains("useCallback")
                || e.contains("useEffect")
                || e.contains("useMemo")
            {
                issue = "hooks".to_string();
            } else if e.contains("() => {")
                || e.contains("function ") && e.contains("(") && e.contains(")") && e.contains("{")
            {
                issue = "function_body".to_string();
            } else {
                // Show first difference
                issue = format!(
                    "other: a='{}' e='{}'",
                    a.chars().take(40).collect::<String>(),
                    e.chars().take(40).collect::<String>()
                );
            }
            break;
        }

        issues.entry(issue).or_default().push(stem);
    }

    eprintln!("Total simple _c(1) no_post_scope: {}", total);
    eprintln!("Already passing: {}", passing);
    eprintln!();

    let mut sorted: Vec<_> = issues.into_iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (category, fixtures) in &sorted {
        eprintln!("{:3} {}", fixtures.len(), category);
        for f in fixtures.iter().take(3) {
            eprintln!("     - {}", f);
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
