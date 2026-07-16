#[cfg(test)]
pub(crate) const EVALUATE_USER_INPUT_WL: &str = include_str!("wl/evaluate_user_input.wl");
pub(crate) const EVALUATE_SCRIPT_SOURCE_WL: &str = include_str!("wl/evaluate_script_source.wl");
pub(crate) const OPTIONS_QUERY_WL: &str = include_str!("wl/options_query.wl");
pub(crate) const SECONDARY_LINK_SETUP_INPUT_WL: &str =
    include_str!("wl/secondary_link_setup_input.wl");
pub(crate) const SYMBOL_COMPLETION_QUERY_WL: &str = include_str!("wl/symbol_completion_query.wl");
pub(crate) const SYMBOL_DEFINITION_QUERY_WL: &str = include_str!("wl/symbol_definition_query.wl");
pub(crate) const SYMBOL_DETAILS_BATCH_QUERY_WL: &str =
    include_str!("wl/symbol_details_batch_query.wl");
pub(crate) const WSTP_EVALUATE_USER_INPUT_WL: &str = include_str!("wl/wstp_evaluate_user_input.wl");

#[cfg(test)]
pub(crate) fn wolfram_user_input_evaluation_expr(input: &str) -> String {
    wolfram_function_call(EVALUATE_USER_INPUT_WL, &[wolfram_string_literal(input)])
}

pub(crate) fn wolfram_function_call(function_source: &str, args: &[String]) -> String {
    format!("({})[{}]", function_source.trim(), args.join(", "))
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

pub(crate) fn wolfram_string_list(values: &[String]) -> String {
    format!(
        "{{{}}}",
        values
            .iter()
            .map(|value| wolfram_string_literal(value))
            .collect::<Vec<_>>()
            .join(", ")
    )
}
