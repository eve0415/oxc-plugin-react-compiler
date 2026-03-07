fn main() {
    let fixture_dir = "third_party/react/compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler";
    let options = oxc_react_compiler::options::PluginOptions::default();

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
            // Check why — is it an arrow function? export default? named export?
            let has_func_decl = source.contains("function Component")
                || source.contains("function component")
                || source.contains("function use")
                || source.contains("function foo")
                || source.contains("function Foo")
                || source.contains("function Test");
            let has_arrow = source.contains("=> {") || source.contains("=>");
            let has_export_default_func = source.contains("export default function");
            let first_line = source.lines().next().unwrap_or("");

            eprintln!(
                "{}: func={}, arrow={}, export_default_func={}",
                stem, has_func_decl, has_arrow, has_export_default_func
            );
            // Show the function-like lines
            for line in source.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("function ")
                    || trimmed.starts_with("const ") && trimmed.contains("=>")
                    || trimmed.starts_with("export default function")
                    || trimmed.starts_with("export function")
                    || trimmed.starts_with("export const")
                {
                    eprintln!("  > {}", trimmed);
                }
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
