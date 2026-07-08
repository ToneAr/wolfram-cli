pub(crate) const EVALUATE_USER_INPUT_WL: &str = include_str!("wl/evaluate_user_input.wl");
pub(crate) const OPTIONS_QUERY_WL: &str = include_str!("wl/options_query.wl");
pub(crate) const SYMBOL_COMPLETION_QUERY_WL: &str = include_str!("wl/symbol_completion_query.wl");
pub(crate) const SYMBOL_DETAILS_BATCH_QUERY_WL: &str =
    include_str!("wl/symbol_details_batch_query.wl");
pub(crate) const TO_EXPRESSION_WITHOUT_SHADOWING_WL: &str =
    include_str!("wl/to_expression_without_shadowing.wl");
pub(crate) const WSTP_EVALUATE_INPUT_TO_STRING_WL: &str =
    include_str!("wl/wstp_evaluate_input_to_string.wl");

pub(crate) fn wolfram_user_input_evaluation_expr(input: &str) -> String {
    EVALUATE_USER_INPUT_WL.replace("__INPUT__", &wolfram_string_literal(input))
}

pub(crate) fn wolfram_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}
