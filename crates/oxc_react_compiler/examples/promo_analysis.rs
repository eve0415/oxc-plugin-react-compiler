fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut would_fix = 0;
    let mut already_correct = 0;
    let mut still_wrong = 0;

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
            already_correct += 1;
            continue;
        }

        // Check if replacing "let t0;" with expected variable name would fix it
        let al: Vec<&str> = actual.lines().collect();
        let el: Vec<&str> = expected.lines().collect();

        // Find the "let t0;" or "let tN;" line and what the expected has instead
        let actual_let = al
            .iter()
            .find(|l| l.starts_with("let t") && l.ends_with(";"));
        let expected_let = el
            .iter()
            .find(|l| l.starts_with("let ") && l.ends_with(";"));

        if let (Some(actual_l), Some(expected_l)) = (actual_let, expected_let) {
            if actual_l != expected_l {
                // Try substituting
                let actual_var = actual_l.trim_start_matches("let ").trim_end_matches(";");
                let expected_var = expected_l.trim_start_matches("let ").trim_end_matches(";");

                let substituted = actual
                    .replace(
                        &format!("let {};", actual_var),
                        &format!("let {};", expected_var),
                    )
                    .replace(
                        &format!("{} = ", actual_var),
                        &format!("{} = ", expected_var),
                    )
                    .replace(
                        &format!("return {};", actual_var),
                        &format!("return {};", expected_var),
                    )
                    .replace(
                        &format!("$[0] = {};", actual_var),
                        &format!("$[0] = {};", expected_var),
                    )
                    .replace(
                        &format!("$[1] = {};", actual_var),
                        &format!("$[1] = {};", expected_var),
                    )
                    .replace(
                        &format!("$[2] = {};", actual_var),
                        &format!("$[2] = {};", expected_var),
                    )
                    .replace(
                        &format!("{} = $[0];", actual_var),
                        &format!("{} = $[0];", expected_var),
                    )
                    .replace(
                        &format!("{} = $[1];", actual_var),
                        &format!("{} = $[1];", expected_var),
                    )
                    .replace(
                        &format!("{} = $[2];", actual_var),
                        &format!("{} = $[2];", expected_var),
                    )
                    .replace(
                        &format!("const {} = {};", expected_var, actual_var),
                        "REMOVE_THIS_LINE",
                    );

                // Remove the "const x = tN;" line that would be redundant
                let substituted_clean: String = substituted
                    .lines()
                    .filter(|l| l.trim() != "REMOVE_THIS_LINE")
                    .collect::<Vec<_>>()
                    .join("\n");

                if substituted_clean == expected {
                    would_fix += 1;
                } else {
                    still_wrong += 1;
                }
            }
        }
    }

    eprintln!("Already correct: {}", already_correct);
    eprintln!("Would fix with better promotion: {}", would_fix);
    eprintln!("Still wrong even with better promotion: {}", still_wrong);
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
