use std::collections::HashSet;

use nu_ansi_term::Style;
use reedline::{Highlighter, StyledText};

use crate::{
    theme::{Theme, ThemeHandle, ThemeStyles},
    wolfram_syntax::{is_symbol_char, is_symbol_start, short_symbol_name},
};

pub(crate) struct WolframHighlighter {
    symbols: HashSet<String>,
    theme: ThemeHandle,
}

impl WolframHighlighter {
    pub(crate) fn new(symbols: HashSet<String>, theme: ThemeHandle) -> Self {
        Self { symbols, theme }
    }
}

impl Highlighter for WolframHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        highlight_wolfram_text(line, self.theme.current().styles(), Some(&self.symbols))
    }
}

pub(crate) fn highlight_wolfram_text(
    line: &str,
    styles: ThemeStyles,
    symbols: Option<&HashSet<String>>,
) -> StyledText {
    let mut out = StyledText::new();
    let mut chars = line.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '"' {
            let start = idx;
            let mut end = idx + ch.len_utf8();
            let mut escaped = false;
            for (next_idx, next) in chars.by_ref() {
                end = next_idx + next.len_utf8();
                if next == '"' && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
                if next != '\\' {
                    escaped = false;
                }
            }
            out.push((styles.string, line[start..end].to_string()));
        } else if ch == '(' && chars.peek().is_some_and(|(_, next)| *next == '*') {
            let start = idx;
            let mut end = idx + ch.len_utf8();
            for (next_idx, next) in chars.by_ref() {
                end = next_idx + next.len_utf8();
                if next == ')' && line[..next_idx].ends_with('*') {
                    break;
                }
            }
            out.push((styles.comment, line[start..end].to_string()));
        } else if ch.is_ascii_digit() {
            let start = idx;
            let mut end = idx + ch.len_utf8();
            while let Some((next_idx, next)) = chars.peek().copied() {
                if next.is_ascii_digit() || next == '.' {
                    chars.next();
                    end = next_idx + next.len_utf8();
                } else {
                    break;
                }
            }
            out.push((styles.number, line[start..end].to_string()));
        } else if is_symbol_start(ch) {
            let start = idx;
            let mut end = idx + ch.len_utf8();
            while let Some((next_idx, next)) = chars.peek().copied() {
                if is_symbol_char(next) {
                    chars.next();
                    end = next_idx + next.len_utf8();
                } else {
                    break;
                }
            }
            let word = &line[start..end];
            let style = if symbols.is_none_or(|symbols| symbols.contains(short_symbol_name(word))) {
                styles.builtin_symbol
            } else {
                Style::new()
            };
            out.push((style, word.to_string()));
        } else {
            out.push((Style::new(), ch.to_string()));
        }
    }

    out
}

pub(crate) fn print_highlighted(text: &str, theme: Theme) {
    for (style, fragment) in highlight_wolfram_text(text, theme.styles(), None).buffer {
        print!("{}", style.paint(fragment));
    }
    println!();
}
