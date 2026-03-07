fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();
    let mut bug_count = 0;
    let mut ok_count = 0;

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
        } // Already passing

        // Check for empty arrow/function bodies in our output
        let has_empty = result.code.contains("=> {}")
            || result.code.contains("=> {\n}")
            || result.code.contains("() => {  }")
            || result.code.contains(") => {  }")
            || result.code.lines().any(|l| {
                let t = l.trim();
                t.ends_with("=> {}") || t.ends_with("=> {  }") || t == "}" // function() {} 
            });

        if !has_empty {
            continue;
        }

        // Does the expected also have these?
        let expected_has = expected_code.lines().any(|l| {
            let t = l.trim();
            t.contains("=> {}") || t.contains("=> {  }")
        });

        if expected_has {
            ok_count += 1;
        } else {
            bug_count += 1;
        }
    }

    eprintln!(
        "Empty body bugs (we have empty, expected doesn't): {}",
        bug_count
    );
    eprintln!("Empty body ok (expected also has empty): {}", ok_count);
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
            s
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}
