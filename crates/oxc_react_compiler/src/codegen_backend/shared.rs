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
    crate::pipeline::strip_trailing_commas_before_closer_for_transform_flag(&normalized_arrows)
}
