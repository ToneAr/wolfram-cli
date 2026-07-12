use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

pub(crate) fn wolfram_input_is_incomplete(line: &str) -> bool {
    let mut stack = Vec::new();
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut comment_depth = 0usize;

    while let Some(ch) = chars.next() {
        if comment_depth > 0 {
            match ch {
                '(' if chars.peek() == Some(&'*') => {
                    chars.next();
                    comment_depth += 1;
                }
                '*' if chars.peek() == Some(&')') => {
                    chars.next();
                    comment_depth -= 1;
                }
                _ => {}
            }
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' if chars.peek() == Some(&'*') => {
                chars.next();
                comment_depth = 1;
            }
            '[' | '{' | '(' => stack.push(ch),
            ']' => {
                if !matches!(stack.pop(), Some('[')) {
                    return false;
                }
            }
            '}' => {
                if !matches!(stack.pop(), Some('{')) {
                    return false;
                }
            }
            ')' if !matches!(stack.pop(), Some('(')) => {
                return false;
            }
            _ => {}
        }
    }

    in_string || comment_depth > 0 || !stack.is_empty()
}

pub(crate) fn symbol_start(line: &str, pos: usize) -> usize {
    line[..pos]
        .rfind(|c: char| !is_symbol_char(c))
        .map_or(0, |idx| idx + 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StringPathCompletionContext {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

pub(crate) fn completion_is_disabled_at_cursor(line: &str, pos: usize) -> bool {
    cursor_is_in_wolfram_string(line, pos) && string_path_completion_context(line, pos).is_none()
}

pub(crate) fn string_path_completion_context(
    line: &str,
    pos: usize,
) -> Option<StringPathCompletionContext> {
    let content_start = wolfram_string_content_start_at_cursor(line, pos)?;
    let before_cursor = line.get(content_start..pos)?;
    let fragment_start = before_cursor
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(content_start, |(idx, ch)| {
            content_start + idx + ch.len_utf8()
        });
    let fragment = line.get(fragment_start..pos)?;

    path_fragment_matches_file_pattern(fragment).then_some(StringPathCompletionContext {
        start: fragment_start,
        end: pos,
    })
}

pub(crate) fn path_fragment_matches_file_pattern(fragment: &str) -> bool {
    !fragment.is_empty()
        && (fragment.starts_with('/')
            || fragment.starts_with("~/")
            || fragment.starts_with("./")
            || fragment.starts_with("../")
            || fragment.contains('/'))
}

pub(crate) fn cursor_is_in_wolfram_string(line: &str, pos: usize) -> bool {
    wolfram_string_content_start_at_cursor(line, pos).is_some()
}

pub(crate) fn wolfram_string_content_start_at_cursor(line: &str, pos: usize) -> Option<usize> {
    let before_cursor = line.get(..pos)?;
    let mut chars = before_cursor.char_indices().peekable();
    let mut string_start = None;
    let mut escaped = false;
    let mut comment_depth = 0usize;

    while let Some((idx, ch)) = chars.next() {
        if comment_depth > 0 {
            match ch {
                '(' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                    chars.next();
                    comment_depth += 1;
                }
                '*' if chars.peek().is_some_and(|(_, next)| *next == ')') => {
                    chars.next();
                    comment_depth -= 1;
                }
                _ => {}
            }
            continue;
        }

        if string_start.is_some() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string_start = None;
            }
            continue;
        }

        match ch {
            '"' => string_start = Some(idx + ch.len_utf8()),
            '(' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                chars.next();
                comment_depth = 1;
            }
            _ => {}
        }
    }

    string_start
}

pub(crate) fn short_symbol_name(symbol: &str) -> &str {
    symbol.rsplit('`').next().unwrap_or(symbol)
}

pub(crate) fn is_symbol_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '$' || ch == '`'
}

pub(crate) fn is_symbol_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '$' || ch == '`'
}

pub(crate) fn is_qualified_symbol_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_symbol_char)
}

pub(crate) fn remember_user_symbols(input: &str, symbols: &Arc<Mutex<HashSet<String>>>) {
    let mut symbols = symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for symbol in user_completion_names(input) {
        symbols.insert(symbol);
    }
}

pub(crate) fn user_completion_names(input: &str) -> Vec<String> {
    let mut names = Vec::new();
    names.extend(context_names(input));
    for segment in input.split([';', '\n']) {
        let trimmed = segment.trim_start();
        let Some((left, _)) = trimmed.split_once('=') else {
            continue;
        };
        if left.ends_with([':', '!', '=', '<', '>']) {
            continue;
        }
        let name: String = left
            .trim_end()
            .chars()
            .take_while(|ch| is_symbol_char(*ch))
            .collect();
        if name.chars().next().is_some_and(is_symbol_start) && is_qualified_symbol_name(&name) {
            if let Some((context, short)) = name.rsplit_once('`') {
                names.push(format!("{context}`"));
                if !short.is_empty() {
                    names.push(name.clone());
                    names.push(short.to_string());
                }
            } else {
                names.push(name);
            }
        }
    }
    names
}

pub(crate) fn context_names(input: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = input;
    while let Some(idx) = rest.find("Begin") {
        rest = &rest[idx + "Begin".len()..];
        if rest.starts_with("Package") {
            rest = &rest["Package".len()..];
        }
        let Some(open) = rest.find('[') else {
            continue;
        };
        let after_open = rest[open + 1..].trim_start();
        let Some(after_quote) = after_open.strip_prefix('"') else {
            continue;
        };
        let Some(close_quote) = after_quote.find('"') else {
            continue;
        };
        let context = &after_quote[..close_quote];
        if is_context_name(context) {
            names.push(context.to_string());
        }
        rest = &after_quote[close_quote + 1..];
    }
    names
}

pub(crate) fn is_context_name(name: &str) -> bool {
    name.ends_with('`') && name.chars().all(is_symbol_char)
}

pub(crate) fn loaded_context_names(input: &str) -> Vec<String> {
    input
        .split("<<")
        .skip(1)
        .filter_map(|rest| {
            let context: String = rest
                .trim_start()
                .chars()
                .take_while(|ch| is_symbol_char(*ch))
                .collect();
            is_context_name(&context).then_some(context)
        })
        .collect()
}

pub(crate) fn option_context(line: &str, pos: usize) -> Option<String> {
    let bracket = innermost_open_square_bracket(line, pos)?;
    let args = &line[bracket + 1..pos];
    if !has_top_level_comma(args) {
        return None;
    }
    symbol_before(line, bracket)
}

pub(crate) fn innermost_open_square_bracket(line: &str, pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in line[..pos].char_indices().rev() {
        match ch {
            ']' | '}' | ')' => depth += 1,
            '[' => {
                if depth == 0 {
                    return Some(idx);
                }
                depth -= 1;
            }
            '{' | '(' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    None
}

pub(crate) fn symbol_before(line: &str, end: usize) -> Option<String> {
    let before = line[..end].trim_end();
    let end = before.len();
    let start = before[..end]
        .rfind(|c: char| !is_symbol_char(c))
        .map_or(0, |idx| idx + 1);
    let symbol = &before[start..end];
    (!symbol.is_empty()).then(|| symbol.to_string())
}

pub(crate) fn has_top_level_comma(input: &str) -> bool {
    top_level_comma_count(input) > 0
}

pub(crate) fn top_level_comma_count(input: &str) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 0usize;

    for ch in input.chars() {
        if in_string {
            if ch == '"' && !escaped {
                in_string = false;
            }
            escaped = ch == '\\' && !escaped;
            if ch != '\\' {
                escaped = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' if depth > 0 => depth -= 1,
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }

    count
}
