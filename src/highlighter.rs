use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};

use crate::{
    completion::{SymbolHighlighterLookup, command_is_on_path},
    theme::{Theme, ThemeHandle, ThemeStyles},
    wolfram_syntax::{is_symbol_char, is_symbol_start, short_symbol_name},
};

pub(crate) struct WolframHighlighter {
    builtin_symbols: &'static HashSet<String>,
    user_symbols: Arc<Mutex<HashSet<String>>>,
    known_qualified_symbols: Option<Arc<Mutex<HashSet<String>>>>,
    symbol_lookup: Option<SymbolHighlighterLookup>,
    theme: ThemeHandle,
    shell_prompt_hidden: Arc<AtomicBool>,
    snapshot: Mutex<SymbolSnapshot>,
}

/// Highlighting runs on every repaint. Locally remembered user symbols are
/// insert-only and can use their length as a change detector. Exact kernel
/// definition results may be removed after the completion epoch changes, so
/// that set is refreshed directly to avoid retaining stale highlighting.
#[derive(Default)]
struct SymbolSnapshot {
    user_len: usize,
    user_symbols: HashSet<String>,
    known_symbols: HashSet<String>,
}

impl WolframHighlighter {
    pub(crate) fn new(
        builtin_symbols: &'static HashSet<String>,
        user_symbols: Arc<Mutex<HashSet<String>>>,
        known_qualified_symbols: Option<Arc<Mutex<HashSet<String>>>>,
        symbol_lookup: Option<SymbolHighlighterLookup>,
        theme: ThemeHandle,
        shell_prompt_hidden: Arc<AtomicBool>,
    ) -> Self {
        Self {
            builtin_symbols,
            user_symbols,
            known_qualified_symbols,
            symbol_lookup,
            theme,
            shell_prompt_hidden,
            snapshot: Mutex::new(SymbolSnapshot::default()),
        }
    }

    fn refreshed_snapshot(&self) -> std::sync::MutexGuard<'_, SymbolSnapshot> {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let user_symbols = self
                .user_symbols
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if user_symbols.len() != snapshot.user_len {
                snapshot.user_symbols = user_symbols.clone();
                snapshot.user_len = user_symbols.len();
            }
        }
        if let Some(known_qualified_symbols) = &self.known_qualified_symbols {
            let known_qualified_symbols = known_qualified_symbols
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.known_symbols = known_qualified_symbols.clone();
        }
        snapshot
    }
}

impl Highlighter for WolframHighlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        self.shell_prompt_hidden.store(
            shell_escape_prompt_is_active(line, cursor),
            Ordering::Relaxed,
        );

        let snapshot = self.refreshed_snapshot();
        highlight_with_symbol_snapshot(
            line,
            cursor,
            self.theme.current().styles(),
            Some(self.builtin_symbols),
            Some(&snapshot.user_symbols),
            self.known_qualified_symbols
                .is_some()
                .then_some(&snapshot.known_symbols),
            self.symbol_lookup.as_ref(),
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
    highlight_wolfram_text_at_cursor(
        line,
        line.len(),
        styles,
        builtin_symbols,
        user_symbols,
        known_qualified_symbols,
        symbol_lookup,
    )
}

pub(crate) fn highlight_wolfram_text_at_cursor(
    line: &str,
    cursor: usize,
    styles: ThemeStyles,
    builtin_symbols: Option<&HashSet<String>>,
    user_symbols: Option<&HashSet<String>>,
    known_qualified_symbols: Option<&HashSet<String>>,
    symbol_lookup: Option<&SymbolHighlighterLookup>,
) -> StyledText {
    highlight_with_symbol_snapshot(
        line,
        cursor,
        styles,
        builtin_symbols,
        user_symbols,
        known_qualified_symbols,
        symbol_lookup,
    )
}

/// Core highlighting pass. `known_symbols` contains exact spellings confirmed
/// by the kernel; qualified names do not implicitly make their short names
/// visible.
fn highlight_with_symbol_snapshot(
    line: &str,
    cursor: usize,
    styles: ThemeStyles,
    builtin_symbols: Option<&HashSet<String>>,
    user_symbols: Option<&HashSet<String>>,
    known_symbols: Option<&HashSet<String>>,
    symbol_lookup: Option<&SymbolHighlighterLookup>,
) -> StyledText {
    if let Some(shell_escape_start) = shell_escape_start(line)
        && shell_escape_is_active(line, cursor, shell_escape_start)
    {
        return highlight_shell_escape(line, styles, shell_escape_start);
    }

    if let Some(command_start) = repl_command_start(line) {
        return highlight_repl_command(line, command_start);
    }

    let mut out = StyledText::new();
    let mut plain = String::new();
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
            flush_plain(&mut out, &mut plain);
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
            flush_plain(&mut out, &mut plain);
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
            flush_plain(&mut out, &mut plain);
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
            let is_builtin = builtin_symbols.is_none_or(|symbols| {
                if let Some((context, name)) = word.rsplit_once('`') {
                    context == "System" && symbols.contains(name)
                } else {
                    symbols.contains(word)
                }
            });
            if !is_builtin {
                symbol_lookup.inspect(|lookup| lookup.request(word));
            }
            let is_user_defined = symbol_lookup.is_none()
                && user_symbols.is_some_and(|symbols| {
                    if word.contains('`') {
                        symbols.contains(word)
                    } else {
                        symbols.contains(short)
                    }
                });
            let is_known_custom_symbol =
                known_symbols.is_some_and(|symbols| symbols.contains(word));
            let style = if is_builtin {
                styles.builtin_symbol
            } else if is_user_defined || is_known_custom_symbol {
                styles.user_symbol
            } else {
                styles.undefined_symbol
            };
            flush_plain(&mut out, &mut plain);
            out.push((style, word.to_string()));
        } else {
            plain.push(ch);
        }
    }

    flush_plain(&mut out, &mut plain);
    out
}

/// Consecutive unstyled characters (whitespace, operators, brackets) are
/// batched into one fragment instead of one heap allocation per character.
fn flush_plain(out: &mut StyledText, plain: &mut String) {
    if !plain.is_empty() {
        out.push((Style::new(), std::mem::take(plain)));
    }
}

fn shell_escape_start(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    trimmed
        .starts_with(":!")
        .then_some(line.len() - trimmed.len())
}

pub(crate) fn shell_escape_prompt_is_active(line: &str, cursor: usize) -> bool {
    shell_escape_start(line).is_some_and(|start| shell_escape_is_active(line, cursor, start))
}

fn shell_escape_is_active(line: &str, cursor: usize, shell_escape_start: usize) -> bool {
    let marker_end = shell_escape_start + 2;
    let cursor = cursor.min(line.len());
    cursor >= marker_end
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
    highlight_shell_escape_with_command_lookup(line, styles, shell_escape_start, command_is_on_path)
}

pub(crate) fn highlight_shell_escape_with_command_lookup(
    line: &str,
    styles: ThemeStyles,
    shell_escape_start: usize,
    command_exists: impl Fn(&str) -> bool,
) -> StyledText {
    let mut out = StyledText::new();
    if shell_escape_start > 0 {
        out.push((Style::new(), line[..shell_escape_start].to_string()));
    }
    out.push((styles.prompt_left, "! ".to_string()));

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
            let token = &command[start..end];
            let style = if Some(start) == first_word_start && !highlighted_command {
                highlighted_command = true;
                if command_exists(token) {
                    styles.completion_command
                } else {
                    Style::new()
                }
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

pub(crate) fn print_highlighted(text: &str, theme: &Theme) {
    for (style, fragment) in
        highlight_wolfram_text(text, theme.styles(), None, None, None, None).buffer
    {
        print!("{}", style.paint(fragment));
    }
    println!();
}
