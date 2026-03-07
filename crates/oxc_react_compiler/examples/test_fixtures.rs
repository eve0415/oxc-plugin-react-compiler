fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let mut options = oxc_react_compiler::options::PluginOptions::default();
    options.compilation_mode = oxc_react_compiler::options::CompilationMode::All;
    options.panic_threshold = oxc_react_compiler::options::PanicThreshold::All;
    let args: Vec<String> = std::env::args().collect();
    let show_diff = args.iter().any(|a| a == "--diff");
    let filter_cat = args
        .iter()
        .position(|a| a == "--cat")
        .and_then(|i| args.get(i + 1));
    let filter_name = args
        .iter()
        .position(|a| a == "--name")
        .and_then(|i| args.get(i + 1));
    let max_show = args
        .iter()
        .position(|a| a == "--max")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(3);
    let full = args.iter().any(|a| a == "--full");

    let mut issues: std::collections::HashMap<String, Vec<(String, String, String, String)>> =
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

        if let Some(name) = filter_name
            && !stem.contains(name.as_str())
        {
            continue;
        }

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

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        let actual = normalize(&result.code);
        let expected = normalize(&expected_code);

        if actual == expected {
            continue;
        }

        let actual_lines: Vec<&str> = actual.lines().collect();
        let expected_lines: Vec<&str> = expected.lines().collect();

        let mut issue = "unknown".to_string();

        if !result.transformed {
            issue = "not_transformed".to_string();
        } else {
            for i in 0..actual_lines.len().max(expected_lines.len()) {
                let a = actual_lines.get(i).unwrap_or(&"<MISSING>");
                let e = expected_lines.get(i).unwrap_or(&"<MISSING>");
                if a == e {
                    continue;
                }

                if e.contains("_temp") {
                    issue = "function_outlining".to_string();
                } else if e.contains("t0 = (") || e.contains("t0 =\n") {
                    issue = "multiline_jsx".to_string();
                } else if e.starts_with("import ")
                    || e.starts_with("const { ")
                    || e.starts_with("const {")
                {
                    issue = "import_mismatch".to_string();
                } else if e.contains("\"use ") {
                    issue = "directive_handling".to_string();
                } else if e.contains("if (")
                    || e.contains("} else")
                    || e.contains("for (")
                    || e.contains("? ")
                {
                    issue = "control_flow".to_string();
                } else if e.contains("x.map(")
                    || e.contains(".map(")
                    || e.contains(".push(")
                    || e.contains(".forEach(")
                {
                    issue = "array_methods".to_string();
                } else if e.contains("useRef")
                    || e.contains("useState")
                    || e.contains("useCallback")
                    || e.contains("useEffect")
                    || e.contains("useMemo")
                    || e.contains("useReducer")
                    || e.contains("useActionState")
                {
                    issue = "hooks".to_string();
                } else if a.contains("let t0;") && e.contains("let t0;") {
                    issue = "scope_structure".to_string();
                } else {
                    issue = format!(
                        "other: expected='{}'",
                        e.chars().take(60).collect::<String>()
                    );
                }
                break;
            }
        }

        // Build diff string
        let mut diff = String::new();
        let max_lines = actual_lines.len().max(expected_lines.len());
        for i in 0..max_lines {
            let a = actual_lines.get(i).copied().unwrap_or("<MISSING>");
            let e = expected_lines.get(i).copied().unwrap_or("<MISSING>");
            if a != e {
                diff.push_str(&format!("  line {}: actual  ='{}'\n", i + 1, a));
                diff.push_str(&format!("  line {}: expected='{}'\n", i + 1, e));
                for j in (i + 1)..((i + 6).min(max_lines)) {
                    let a2 = actual_lines.get(j).copied().unwrap_or("<MISSING>");
                    let e2 = expected_lines.get(j).copied().unwrap_or("<MISSING>");
                    if a2 != e2 {
                        diff.push_str(&format!("  line {}: actual  ='{}'\n", j + 1, a2));
                        diff.push_str(&format!("  line {}: expected='{}'\n", j + 1, e2));
                    }
                }
                break;
            }
        }

        issues
            .entry(issue)
            .or_default()
            .push((stem, actual, expected, diff));
    }

    let mut sorted: Vec<_> = issues.into_iter().collect();
    sorted.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));

    for (category, fixtures) in &sorted {
        if let Some(cat) = filter_cat
            && !category.contains(cat.as_str())
        {
            continue;
        }
        eprintln!("{:3} {}", fixtures.len(), category);
        for (name, actual, expected, diff) in fixtures.iter().take(max_show) {
            eprintln!("     - {}", name);
            if show_diff {
                eprintln!("{}", diff);
            }
            if full {
                eprintln!("\n  === ACTUAL ===");
                for (i, line) in actual.lines().enumerate() {
                    eprintln!("  {:3}: {}", i + 1, line);
                }
                eprintln!("  === EXPECTED ===");
                for (i, line) in expected.lines().enumerate() {
                    eprintln!("  {:3}: {}", i + 1, line);
                }
                eprintln!();
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
