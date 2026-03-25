pub(crate) fn normalize_for_transform_flag(code: &str) -> String {
    let compact: String = code
        .replace("\r\n", "\n")
        .trim_end()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    let normalized_quotes = compact.replace('\'', "\"");
    let normalized_arrows =
        crate::pipeline::strip_single_param_arrow_parens_for_transform_flag(&normalized_quotes);
    let normalized_trailing_commas =
        crate::pipeline::strip_trailing_commas_before_closer_for_transform_flag(&normalized_arrows);
    strip_wrapped_function_initializer_parens(&normalized_trailing_commas)
}

fn strip_wrapped_function_initializer_parens(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut result = String::with_capacity(code.len());
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] == b'='
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'('
            && let Some(close_idx) = find_matching_paren(code, i + 1)
        {
            let inner = &code[i + 2..close_idx];
            let next = bytes.get(close_idx + 1).copied();
            if next.is_some_and(is_initializer_terminator)
                && looks_like_wrapped_function_expression(inner)
            {
                result.push('=');
                result.push_str(inner);
                i = close_idx + 1;
                continue;
            }
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

fn looks_like_wrapped_function_expression(inner: &str) -> bool {
    inner.starts_with("async()=>")
        || inner.starts_with("()=>")
        || inner.starts_with("async(")
        || inner.starts_with('(') && inner.contains("=>")
        || inner.starts_with("function")
        || inner.starts_with("asyncfunction")
}

fn is_initializer_terminator(byte: u8) -> bool {
    matches!(byte, b';' | b',' | b'}' | b')')
}

fn find_matching_paren(code: &str, open_idx: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut i = open_idx;
    let mut in_string: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let byte = bytes[i];

        if let Some(quote) = in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == quote {
                in_string = None;
            }
            i += 1;
            continue;
        }

        match byte {
            b'"' | b'\'' | b'`' => in_string = Some(byte),
            b'(' => paren_depth += 1,
            b')' => {
                paren_depth = paren_depth.checked_sub(1)?;
                if paren_depth == 0 && brace_depth == 0 && bracket_depth == 0 {
                    return Some(i);
                }
            }
            b'{' => brace_depth += 1,
            b'}' => brace_depth = brace_depth.checked_sub(1)?,
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.checked_sub(1)?,
            _ => {}
        }

        i += 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_whitespace_and_quotes() {
        let code = "const x = 'hello';\n  const y = 42;\n";
        let result = normalize_for_transform_flag(code);
        assert!(
            !result.contains(' '),
            "whitespace should be stripped, got: {result}"
        );
        assert!(
            !result.contains('\''),
            "single quotes should be normalized to double, got: {result}"
        );
    }

    #[test]
    fn normalize_unwraps_function_parens() {
        let code = "constx=(function(){});";
        let result = normalize_for_transform_flag(code);
        assert!(
            !result.contains("=(function"),
            "wrapped function expression parens should be stripped, got: {result}"
        );
        assert!(
            result.contains("=function"),
            "function expression should remain after unwrap, got: {result}"
        );
    }
}
