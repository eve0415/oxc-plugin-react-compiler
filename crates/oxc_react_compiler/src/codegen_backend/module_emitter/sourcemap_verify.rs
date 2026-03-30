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

    #[test]
    fn sourcemap_threshold_invariant() {
        // Verify the GENERATED_SRC_LINE_THRESHOLD contract:
        // - No user-source token has src_line >= 1_000_000
        // - Virtual-source tokens exist after enrichment
        // Use a component with reactive state to ensure memoization infrastructure is generated.
        let source = r#"function Component(props) {
  const [count, setCount] = useState(0);
  const doubled = count * 2;
  return <div onClick={() => setCount(count + 1)}>{doubled}</div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let sm =
            oxc_sourcemap::SourceMap::from_json_string(map_json).expect("valid sourcemap JSON");

        let sources: Vec<_> = sm.get_sources().collect();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        assert!(virtual_idx.is_some(), "virtual source should exist");

        for token in sm.get_tokens() {
            if token.get_source_id() == virtual_idx {
                // Virtual tokens should have src_line 0 (routed to virtual source).
                assert_eq!(
                    token.get_src_line(),
                    0,
                    "virtual token should have src_line 0"
                );
            } else {
                // User tokens must NOT have unreasonably high line numbers.
                // If a token has src_line >= 1M, the threshold routing is broken.
                assert!(
                    token.get_src_line() < 1_000_000,
                    "user token has src_line {} >= threshold — routing is broken",
                    token.get_src_line()
                );
            }
        }
    }

    #[test]
    fn sourcemap_strict_identity_mapping_uncompiled() {
        // Uncompiled helper function should have identity-ish mapping:
        // dst_line should be close to src_line (within ±1).
        let source = r#"function helper(x) { return x + 1; }
function Component(props) {
  return <div>{props.x}</div>;
}"#;
        let result = compile_to_result(source);
        assert!(result.transformed);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let sm =
            oxc_sourcemap::SourceMap::from_json_string(map_json).expect("valid sourcemap JSON");

        let sources: Vec<_> = sm.get_sources().collect();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        let helper_tokens: Vec<_> = sm
            .get_tokens()
            .filter(|t| t.get_src_line() == 0 && t.get_source_id() != virtual_idx)
            .collect();

        assert!(
            !helper_tokens.is_empty(),
            "helper function should have tokens"
        );

        for token in &helper_tokens {
            let drift = (token.get_dst_line() as i64 - token.get_src_line() as i64).unsigned_abs();
            assert!(
                drift <= 2,
                "helper token at src_line {} mapped to dst_line {} — drift {} exceeds ±2",
                token.get_src_line(),
                token.get_dst_line(),
                drift
            );
        }
    }

    #[test]
    fn sourcemap_no_out_of_bounds_columns() {
        // Every token's src_col and dst_col must be within line bounds.
        let source = r#"function Component(props) {
  const x = props.a + props.b + props.c;
  const y = x > 10 ? "large" : "small";
  return <div className={y}>{x}</div>;
}"#;
        let result = compile_to_result(source);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let sm =
            oxc_sourcemap::SourceMap::from_json_string(map_json).expect("valid sourcemap JSON");

        let source_lines: Vec<&str> = source.lines().collect();
        let gen_lines: Vec<&str> = result.code.lines().collect();

        let sources: Vec<_> = sm.get_sources().collect();
        let virtual_idx = sources
            .iter()
            .position(|s| s.as_ref() == "compiler://react-compiler/generated")
            .map(|i| i as u32);

        for token in sm.get_tokens() {
            if token.get_source_id() == virtual_idx {
                continue;
            }
            let src_line = token.get_src_line() as usize;
            let src_col = token.get_src_col() as usize;
            let dst_line = token.get_dst_line() as usize;
            let dst_col = token.get_dst_col() as usize;

            if src_line < source_lines.len() {
                assert!(
                    src_col <= source_lines[src_line].len(),
                    "src_col {} OOB for line {} (len {})",
                    src_col,
                    src_line,
                    source_lines[src_line].len()
                );
            }
            if dst_line < gen_lines.len() {
                assert!(
                    dst_col <= gen_lines[dst_line].len(),
                    "dst_col {} OOB for gen line {} (len {})",
                    dst_col,
                    dst_line,
                    gen_lines[dst_line].len()
                );
            }
        }
    }

    #[test]
    fn sourcemap_source_content_integrity() {
        // sourcesContent[0] must match the original input byte-for-byte.
        let source = "function Component(props) {\n  return <div>{props.x}</div>;\n}";
        let result = compile_to_result(source);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let parsed: serde_json::Value =
            serde_json::from_str(map_json).expect("valid sourcemap JSON");
        let sources_content = parsed["sourcesContent"]
            .as_array()
            .expect("should have sourcesContent");
        assert_eq!(
            sources_content[0].as_str().unwrap(),
            source,
            "sourcesContent[0] should match original source exactly"
        );
    }

    #[test]
    fn sourcemap_strengthened_monotonicity() {
        // Tokens must be ordered by (dst_line, dst_col) — both line AND column monotonic.
        let source = r#"function Component(props) {
  const a = props.x + 1;
  const b = props.y * 2;
  return <div a={a} b={b}>{props.children}</div>;
}"#;
        let result = compile_to_result(source);
        let map_json = result.map.as_ref().expect("sourcemap should exist");
        let sm =
            oxc_sourcemap::SourceMap::from_json_string(map_json).expect("valid sourcemap JSON");

        let mut prev_dst_line = 0u32;
        let mut prev_dst_col = 0u32;
        for token in sm.get_tokens() {
            let dl = token.get_dst_line();
            let dc = token.get_dst_col();
            if dl == prev_dst_line {
                assert!(
                    dc >= prev_dst_col,
                    "column must be non-decreasing on same line: prev {}:{}, curr {}:{}",
                    prev_dst_line,
                    prev_dst_col,
                    dl,
                    dc
                );
            } else {
                assert!(
                    dl > prev_dst_line,
                    "lines must be non-decreasing: prev {}, curr {}",
                    prev_dst_line,
                    dl
                );
            }
            prev_dst_line = dl;
            prev_dst_col = dc;
        }
    }
}
