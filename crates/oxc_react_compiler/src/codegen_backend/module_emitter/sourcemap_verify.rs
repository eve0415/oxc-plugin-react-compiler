//! End-to-end sourcemap verification (run with `cargo test -- sourcemap_e2e --nocapture`)

#[cfg(test)]
mod tests {
    use crate::test_utils::compile_to_result;

    #[test]
    fn sourcemap_e2e_verification() {
        let source = r#"function Component(props) {
  const [count, setCount] = useState(0);
  const doubled = count * 2;
  const onClick = () => {
    setCount(count + 1);
  };
  return (
    <div>
      <span>{doubled}</span>
      <button onClick={onClick}>+1</button>
    </div>
  );
}"#;

        let result = compile_to_result(source);
        assert!(result.transformed, "should be transformed");

        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let sm =
            oxc_sourcemap::SourceMap::from_json_string(map_json).expect("valid sourcemap JSON");

        let source_lines: Vec<&str> = source.lines().collect();
        let gen_lines: Vec<&str> = result.code.lines().collect();

        eprintln!("=== SOURCE ({} lines) ===", source_lines.len());
        for (i, line) in source_lines.iter().enumerate() {
            eprintln!("  src:{i:3} | {line}");
        }

        eprintln!("\n=== GENERATED ({} lines) ===", gen_lines.len());
        for (i, line) in gen_lines.iter().enumerate() {
            eprintln!("  gen:{i:3} | {line}");
        }

        let sources: Vec<_> = sm.get_sources().collect();
        eprintln!("\n=== SOURCES ===");
        for (i, s) in sources.iter().enumerate() {
            eprintln!("  [{i}] {s}");
        }

        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated");

        eprintln!(
            "\n=== TOKEN MAPPINGS ({} tokens) ===",
            sm.get_tokens().count()
        );
        eprintln!("  {:>8} {:>8}  source", "gen", "src");
        let mut user_count = 0;
        let mut gen_count = 0;
        for token in sm.get_tokens() {
            let is_virtual = token.get_source_id() == virtual_idx.map(|i| i as u32);
            let source_label = if is_virtual {
                gen_count += 1;
                "<generated>".to_string()
            } else {
                user_count += 1;
                let src_line = token.get_src_line() as usize;
                if src_line < source_lines.len() {
                    let line_preview = source_lines[src_line];
                    let col = token.get_src_col() as usize;
                    let preview = if col < line_preview.len() {
                        &line_preview[col..col.saturating_add(30).min(line_preview.len())]
                    } else {
                        "<end>"
                    };
                    format!("\"{}\"", preview)
                } else {
                    format!("line {} (out of range)", src_line)
                }
            };
            eprintln!(
                "  {:3}:{:<4} {:3}:{:<4}  {}",
                token.get_dst_line(),
                token.get_dst_col(),
                token.get_src_line(),
                token.get_src_col(),
                source_label
            );
        }

        eprintln!("\n=== SUMMARY ===");
        eprintln!("  User tokens: {user_count}");
        eprintln!("  Generated tokens: {gen_count}");
        eprintln!("  Total: {}", user_count + gen_count);

        // Verify the sourcemap has the expected properties
        let parsed: serde_json::Value = serde_json::from_str(map_json).unwrap();
        assert!(parsed["debugId"].is_string(), "should have debugId");
        assert!(
            parsed["x_google_ignoreList"].is_array(),
            "should have x_google_ignoreList"
        );
        assert!(parsed["version"] == 3, "version should be 3");
        assert!(user_count > 0, "should have user-source tokens");
        assert!(
            virtual_idx.is_some(),
            "should have virtual generated source"
        );
    }
}
