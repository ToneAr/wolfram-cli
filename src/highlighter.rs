use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

use crate::{
    completion::SymbolHighlighterLookup,
    theme::{Theme, ThemeHandle, ThemeStyles},
    wolfram_syntax::{is_symbol_char, is_symbol_start, short_symbol_name},
};

pub(crate) struct WolframHighlighter {
    builtin_symbols: &'static HashSet<String>,
    user_symbols: Arc<Mutex<HashSet<String>>>,
    known_qualified_symbols: Arc<Mutex<HashSet<String>>>,
    symbol_lookup: SymbolHighlighterLookup,
    theme: ThemeHandle,
}

impl WolframHighlighter {
    pub(crate) fn new(
        builtin_symbols: &'static HashSet<String>,
        user_symbols: Arc<Mutex<HashSet<String>>>,
        known_qualified_symbols: Arc<Mutex<HashSet<String>>>,
        symbol_lookup: SymbolHighlighterLookup,
        theme: ThemeHandle,
    ) -> Self {
        Self {
            builtin_symbols,
            user_symbols,
            known_qualified_symbols,
            symbol_lookup,
            theme,
        }
    }
}

impl Highlighter for WolframHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let user_symbols = self
            .user_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let known_qualified_symbols = self
            .known_qualified_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        highlight_wolfram_text(
            line,
            self.theme.current().styles(),
            Some(&self.builtin_symbols),
            Some(&user_symbols),
            Some(&known_qualified_symbols),
            Some(&self.symbol_lookup),
        )
    }
}

pub(crate) fn highlight_wolfram_text(
    line: &str,
    styles: ThemeStyles,
    builtin_symbols: Option<&HashSet<String>>,
    user_symbols: Option<&HashSet<String>>,
    known_qualified_symbols: Option<&HashSet<String>>,
    symbol_lookup: Option<&SymbolHighlighterLookup>,
) -> StyledText {
    if let Some(shell_escape_start) = shell_escape_start(line) {
        return highlight_shell_escape(line, styles, shell_escape_start);
    }

    if let Some(command_start) = repl_command_start(line) {
        return highlight_repl_command(line, command_start);
    }

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
            let short = short_symbol_name(word);
            let is_builtin = word.starts_with("System`")
                || builtin_symbols.is_none_or(|symbols| symbols.contains(short));
            if !is_builtin && (is_non_global_or_internal_symbol(word) || !word.contains('`')) {
                symbol_lookup.inspect(|lookup| lookup.request(word));
            }
            let is_user_defined = user_symbols.is_some_and(|symbols| {
                if word.contains('`') {
                    is_non_global_or_internal_symbol(word) && symbols.contains(word)
                } else {
                    symbols.contains(short)
                }
            });
            let is_known_custom_symbol = known_qualified_symbols.is_some_and(|symbols| {
                if word.contains('`') {
                    is_non_global_or_internal_symbol(word) && symbols.contains(word)
                } else {
                    symbols
                        .iter()
                        .any(|symbol| short_symbol_name(symbol) == word)
                }
            });
            let style = if is_builtin {
                styles.builtin_symbol
            } else if is_user_defined || is_known_custom_symbol {
                styles.user_symbol
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

fn shell_escape_start(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    trimmed
        .starts_with(":!")
        .then_some(line.len() - trimmed.len())
}

fn repl_command_start(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    trimmed
        .starts_with(':')
        .then_some(line.len() - trimmed.len())
}

fn highlight_repl_command(line: &str, command_start: usize) -> StyledText {
    let mut out = StyledText::new();
    if command_start > 0 {
        out.push((Style::new(), line[..command_start].to_string()));
    }
    out.push((Style::new().fg(Color::Cyan), ":".to_string()));
    let command_text_start = command_start + 1;
    if command_text_start < line.len() {
        out.push((Style::new(), line[command_text_start..].to_string()));
    }
    out
}

fn highlight_shell_escape(
    line: &str,
    styles: ThemeStyles,
    shell_escape_start: usize,
) -> StyledText {
    let mut out = StyledText::new();
    if shell_escape_start > 0 {
        out.push((Style::new(), line[..shell_escape_start].to_string()));
    }
    out.push((Style::new().fg(Color::Red).bold(), "!".to_string()));

    let command_start = shell_escape_start + 2;
    let command = &line[command_start..];
    let first_word_start = command
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map(|(idx, _)| idx);
    let mut highlighted_command = false;
    let mut chars = command.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if ch == '\'' || ch == '"' {
            let start = idx;
            let quote = ch;
            let mut end = idx + ch.len_utf8();
            let mut escaped = false;
            for (next_idx, next) in chars.by_ref() {
                end = next_idx + next.len_utf8();
                if next == quote && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
                if next != '\\' {
                    escaped = false;
                }
            }
            out.push((styles.string, command[start..end].to_string()));
        } else if ch == '#' {
            out.push((styles.comment, command[idx..].to_string()));
            break;
        } else if !ch.is_whitespace() && (Some(idx) == first_word_start || ch == '-') {
            let start = idx;
            let mut end = idx + ch.len_utf8();
            while let Some((next_idx, next)) = chars.peek().copied() {
                if next.is_whitespace() || matches!(next, '\'' | '"' | '#') {
                    break;
                }
                chars.next();
                end = next_idx + next.len_utf8();
            }
            let style = if Some(start) == first_word_start && !highlighted_command {
                highlighted_command = true;
                styles.completion_command
            } else {
                styles.completion_option
            };
            out.push((style, command[start..end].to_string()));
        } else {
            out.push((Style::new(), ch.to_string()));
        }
    }

    out
}

fn is_non_global_or_internal_symbol(symbol: &str) -> bool {
    symbol.rsplit_once('`').is_some_and(|(context, name)| {
        !name.is_empty() && !matches!(context, "Global" | "Internal")
    })
}

pub(crate) fn print_highlighted(text: &str, theme: &Theme) {
    for (style, fragment) in
        highlight_wolfram_text(text, theme.styles(), None, None, None, None).buffer
    {
        print!("{}", style.paint(fragment));
    }
    println!();
}
