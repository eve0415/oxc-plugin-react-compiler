fn main() {
    let args: Vec<String> = std::env::args().collect();
    let target = args.get(1).expect("usage: show_one <fixture_stem>");

    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

    for ext in &["js", "jsx", "ts", "tsx"] {
        let path = format!("{}/{}.{}", fixture_dir, target, ext);
        if !std::path::Path::new(&path).exists() {
            continue;
        }

        let source = std::fs::read_to_string(&path).unwrap();
        let expect_path = format!("{}/{}.expect.md", fixture_dir, target);
        let expect_md = std::fs::read_to_string(&expect_path).unwrap();
        let expected_code = extract_code_block(&expect_md).unwrap();

        let filename = format!("{}.{}", target, ext);
        let result = oxc_react_compiler::compile(&filename, &source, &options);

        let actual = normalize(&result.code);
        let expected = normalize(&expected_code);

        let al: Vec<&str> = actual.lines().collect();
        let el: Vec<&str> = expected.lines().collect();
        let max = al.len().max(el.len());

        for i in 0..max {
            let a = al.get(i).unwrap_or(&"<MISSING>");
            let e = el.get(i).unwrap_or(&"<MISSING>");
            if a == e {
                eprintln!("   {:3} {}", i + 1, a);
            } else {
                eprintln!(" A {:3} {}", i + 1, a);
                eprintln!(" E {:3} {}", i + 1, e);
            }
        }
        return;
    }
    eprintln!("Not found: {}", target);
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
