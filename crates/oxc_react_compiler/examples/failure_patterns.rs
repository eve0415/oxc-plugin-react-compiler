fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let mut options = oxc_react_compiler::options::PluginOptions::default();
    options.compilation_mode = oxc_react_compiler::options::CompilationMode::All;

    let mut total_fail = 0;
    let mut no_transform = 0;
    let mut wrong_cache_size = 0;
    let mut cache_size_diffs: std::collections::HashMap<i32, u32> =
        std::collections::HashMap::new();
    let mut false_memo = 0; // we produce cache but expected doesn't have _c(
    let mut missing_memo = 0; // expected has _c( but we don't produce cache
    let mut body_diff = 0;

    // Track specific patterns in body diffs
    let mut has_wrong_property_access = 0; // .unknown or wrong property
    let mut has_missing_ternary = 0;
    let mut has_missing_logical = 0;
    let mut has_wrong_arrow = 0;
    let mut has_extra_statements = 0; // our output has more statements
    let mut has_fewer_statements = 0; // our output has fewer statements

    // Track close-to-passing fixtures: (diff_lines, name, diff_summary)
    let mut close_fixtures: Vec<(usize, String, String)> = Vec::new();

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

        let filename = path.file_name().unwrap().to_string_lossy().to_string();
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        if !result.transformed {
            if expected_code.contains("_c(") {
                no_transform += 1;
            }
            continue;
        }

        let actual = normalize_code(&result.code);
        let expected = normalize_code(&expected_code);
        if actual == expected {
            continue;
        }

        total_fail += 1;

        if !expected_code.contains("_c(") {
            false_memo += 1;
            continue;
        }

        if !result.code.contains("_c(") {
            missing_memo += 1;
            continue;
        }

        let a_size = extract_cache_size(&actual);
        let e_size = extract_cache_size(&expected);

        if a_size != e_size {
            wrong_cache_size += 1;
            if let (Some(a), Some(e)) = (a_size, e_size) {
                *cache_size_diffs.entry(a as i32 - e as i32).or_insert(0) += 1;
            }
            continue;
        }

        body_diff += 1;

        // Analyze what's different in the body
        let a_lines: Vec<&str> = actual.lines().collect();
        let e_lines: Vec<&str> = expected.lines().collect();

        // Count differing lines
        let max_lines = a_lines.len().max(e_lines.len());
        let mut diff_count = 0;
        let mut diff_lines: Vec<String> = Vec::new();
        for i in 0..max_lines {
            let a_line = a_lines.get(i).unwrap_or(&"");
            let e_line = e_lines.get(i).unwrap_or(&"");
            if a_line != e_line {
                diff_count += 1;
                if diff_lines.len() < 5 {
                    diff_lines.push(format!("  E: {}", e_line));
                    diff_lines.push(format!("  A: {}", a_line));
                }
            }
        }
        close_fixtures.push((diff_count, stem.clone(), diff_lines.join("\n")));

        // Check for specific patterns
        if actual.contains(".unknown") || actual.contains("[unknown]") {
            has_wrong_property_access += 1;
        }
        if expected.contains(" ? ") && !actual.contains(" ? ") {
            has_missing_ternary += 1;
        }
        if (expected.contains(" && ") || expected.contains(" || ") || expected.contains(" ?? "))
            && !actual.contains(" && ")
            && !actual.contains(" || ")
            && !actual.contains(" ?? ")
        {
            has_missing_logical += 1;
        }
        if expected.contains("=> ") && !actual.contains("=> ") {
            has_wrong_arrow += 1;
        }
        if a_lines.len() > e_lines.len() + 2 {
            has_extra_statements += 1;
        }
        if a_lines.len() + 2 < e_lines.len() {
            has_fewer_statements += 1;
        }
    }

    eprintln!("=== Failure Analysis ===");
    eprintln!("Total failing: {}", total_fail);
    eprintln!();
    eprintln!("Categories:");
    eprintln!("  no_transform (expected _c but we skip): {}", no_transform);
    eprintln!(
        "  false_memo (we memoize, expected doesn't): {}",
        false_memo
    );
    eprintln!(
        "  missing_memo (expected memoizes, we don't): {}",
        missing_memo
    );
    eprintln!("  wrong_cache_size: {}", wrong_cache_size);
    let mut cache_diffs_sorted: Vec<_> = cache_size_diffs.into_iter().collect();
    cache_diffs_sorted.sort_by_key(|(k, _)| *k);
    eprintln!("  cache size diff distribution (actual - expected):");
    for (diff, count) in &cache_diffs_sorted {
        eprintln!("    off by {:+}: {} fixtures", diff, count);
    }
    eprintln!("  body_diff (same cache size): {}", body_diff);
    eprintln!();
    eprintln!("Body diff patterns:");
    eprintln!("  wrong property (.unknown): {}", has_wrong_property_access);
    eprintln!("  missing ternary: {}", has_missing_ternary);
    eprintln!("  missing logical: {}", has_missing_logical);
    eprintln!("  wrong arrow format: {}", has_wrong_arrow);
    eprintln!(
        "  extra statements (>2 more lines): {}",
        has_extra_statements
    );
    eprintln!(
        "  fewer statements (>2 fewer lines): {}",
        has_fewer_statements
    );

    // Sort by diff count and show the closest fixtures
    close_fixtures.sort_by_key(|(diff, _, _)| *diff);
    eprintln!("\n=== Closest to passing (same cache size, sorted by line diff count) ===");
    for (diff, name, details) in close_fixtures.iter().take(30) {
        eprintln!("  {} ({} differing lines):", name, diff);
        eprintln!("{}", details);
    }
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
                // Recursively normalize nested content
                let inner_normalized = normalize_destructuring(inner_trimmed);
                result.push_str(&format!("{{ {} }}", inner_normalized));
                i = j;
                continue;
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
