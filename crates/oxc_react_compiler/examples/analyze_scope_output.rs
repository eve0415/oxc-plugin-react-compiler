/// Analyze _c(1) fixtures to understand the scope output pattern.
/// Categorize by: what is cached in $[0] and what's after the scope.
fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";

    let mut patterns: std::collections::HashMap<String, Vec<String>> =
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

        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let Some(expected_code) = extract_code_block(&expect_md) else {
            continue;
        };

        if !expected_code.contains("_c(1)") || expected_code.contains("_c(2)") {
            continue;
        }

        // Find the pattern:
        // 1. What variable is before the scope? (e.g., "let t0;" or "let x;")
        // 2. What's in $[0]? (e.g., "$[0] = t0;" or "$[0] = x;")
        // 3. Is there code between "}" (end of else) and "return"?

        let lines: Vec<&str> = expected_code
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();

        // Find the "} else {" line (end of if body)
        let else_line = lines.iter().position(|l| *l == "} else {");
        let Some(else_idx) = else_line else {
            continue;
        };

        // Find the closing "}" of else
        let after_else = lines.get(else_idx + 2); // should be "}"
        if after_else != Some(&"}") {
            continue;
        }
        let close_else_idx = else_idx + 2;

        // Check what's between close_else and return
        let return_line = lines.iter().position(|l| l.starts_with("return "));
        let Some(return_idx) = return_line else {
            continue;
        };

        if close_else_idx + 1 > return_idx {
            continue;
        }
        let post_scope_lines: Vec<&&str> = lines[close_else_idx + 1..return_idx].iter().collect();

        let pattern = if post_scope_lines.is_empty() {
            "no_post_scope".to_string()
        } else {
            let joined: String = post_scope_lines
                .iter()
                .map(|l| (**l).to_string())
                .collect::<Vec<String>>()
                .join(" | ");
            format!(
                "post_scope: {}",
                joined.chars().take(80).collect::<String>()
            )
        };

        patterns.entry(pattern).or_default().push(stem);
    }

    let mut sorted: Vec<_> = patterns.into_iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (pattern, fixtures) in &sorted {
        eprintln!("{:3} {}", fixtures.len(), pattern);
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
