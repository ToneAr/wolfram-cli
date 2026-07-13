use std::collections::HashSet;

use nu_ansi_term::Style;
use reedline::StyledText;
use tree_sitter::{Node, Parser};

use crate::theme::ThemeStyles;

unsafe extern "C" {
    fn tree_sitter_wolfram() -> tree_sitter::Language;
}

fn language() -> tree_sitter::Language {
    // SAFETY: `tree_sitter_wolfram` is provided by the vendored generated
    // grammar compiled in build.rs. It returns a static Tree-sitter language
    // descriptor and has no preconditions.
    unsafe { tree_sitter_wolfram() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SemanticKind {
    LocalVariable,
    PatternVariable,
}

impl SemanticKind {
    fn style(self, styles: ThemeStyles) -> Style {
        match self {
            Self::LocalVariable => styles.semantic_local_variable,
            Self::PatternVariable => styles.semantic_pattern_variable,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SemanticSpan {
    start: usize,
    end: usize,
    kind: SemanticKind,
}

pub(crate) fn enhance_highlighting(line: &str, base: StyledText, styles: ThemeStyles) -> StyledText {
    let spans = semantic_spans(line);
    if spans.is_empty() {
        return base;
    }
    overlay_semantic_spans(line, base, &spans, styles)
}

pub(crate) fn semantic_spans(line: &str) -> Vec<SemanticSpan> {
    let mut parser = Parser::new();
    if parser.set_language(&language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(line, None) else {
        return Vec::new();
    };

    let mut spans = Vec::new();
    collect_semantic_spans(tree.root_node(), line.as_bytes(), &mut spans);
    spans.sort_by_key(|span| (span.start, span.end));
    spans.dedup_by_key(|span| (span.start, span.end, span.kind));
    spans
}

fn collect_semantic_spans(node: Node<'_>, source: &[u8], spans: &mut Vec<SemanticSpan>) {
    match node.kind() {
        "call" => collect_call_spans(node, source, spans),
        "pattern" => collect_pattern_span(node, spans),
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_semantic_spans(child, source, spans);
    }
}

fn collect_call_spans(node: Node<'_>, source: &[u8], spans: &mut Vec<SemanticSpan>) {
    let Some(head) = node.child_by_field_name("head") else {
        return;
    };
    if head.kind() != "symbol" {
        return;
    }
    let Ok(head_text) = head.utf8_text(source) else {
        return;
    };
    if !matches!(head_text, "Module" | "Block" | "With") {
        return;
    }

    let Some(arguments) = node.child_by_field_name("arguments") else {
        return;
    };
    let Some(local_list) = first_named_child(arguments) else {
        return;
    };

    let local_names = collect_local_symbols_from_list(local_list, source, spans);
    if local_names.is_empty() {
        return;
    }

    let mut cursor = arguments.walk();
    for argument in arguments.named_children(&mut cursor).skip(1) {
        collect_symbol_uses(argument, source, &local_names, spans);
    }
}

fn collect_pattern_span(node: Node<'_>, spans: &mut Vec<SemanticSpan>) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    if name.kind() == "symbol" {
        spans.push(SemanticSpan {
            start: name.start_byte(),
            end: name.end_byte(),
            kind: SemanticKind::PatternVariable,
        });
    }
}

fn collect_local_symbols_from_list(
    node: Node<'_>,
    source: &[u8],
    spans: &mut Vec<SemanticSpan>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    collect_local_declaration_nodes(node, source, spans, &mut names);
    names
}

fn collect_local_declaration_nodes(
    node: Node<'_>,
    source: &[u8],
    spans: &mut Vec<SemanticSpan>,
    names: &mut HashSet<String>,
) {
    match node.kind() {
        "symbol" => push_local_span(node, source, spans, names),
        "binary" | "call" => {
            if let Some(symbol) = first_named_descendant_of_kind(node, "symbol") {
                push_local_span(symbol, source, spans, names);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_local_declaration_nodes(child, source, spans, names);
            }
        }
    }
}

fn push_local_span(
    symbol: Node<'_>,
    source: &[u8],
    spans: &mut Vec<SemanticSpan>,
    names: &mut HashSet<String>,
) {
    spans.push(SemanticSpan {
        start: symbol.start_byte(),
        end: symbol.end_byte(),
        kind: SemanticKind::LocalVariable,
    });
    if let Ok(text) = symbol.utf8_text(source) {
        names.insert(text.to_string());
    }
}

fn collect_symbol_uses(
    node: Node<'_>,
    source: &[u8],
    local_names: &HashSet<String>,
    spans: &mut Vec<SemanticSpan>,
) {
    if node.kind() == "symbol"
        && node
            .utf8_text(source)
            .is_ok_and(|text| local_names.contains(text))
    {
        spans.push(SemanticSpan {
            start: node.start_byte(),
            end: node.end_byte(),
            kind: SemanticKind::LocalVariable,
        });
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_symbol_uses(child, source, local_names, spans);
    }
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn first_named_descendant_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    if node.kind() == kind {
        return Some(node);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_named_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

fn overlay_semantic_spans(
    line: &str,
    base: StyledText,
    spans: &[SemanticSpan],
    styles: ThemeStyles,
) -> StyledText {
    let mut out = StyledText::new();
    let mut base_offset = 0;
    let mut span_index = 0;

    for (base_style, fragment) in base.buffer {
        let fragment_start = base_offset;
        let fragment_end = fragment_start + fragment.len();
        base_offset = fragment_end;

        let mut cursor = fragment_start;
        while cursor < fragment_end {
            while span_index < spans.len() && spans[span_index].end <= cursor {
                span_index += 1;
            }

            let Some(span) = spans.get(span_index).copied() else {
                push_fragment(&mut out, base_style, &line[cursor..fragment_end]);
                break;
            };

            if span.start >= fragment_end {
                push_fragment(&mut out, base_style, &line[cursor..fragment_end]);
                break;
            }

            if cursor < span.start {
                let end = span.start.min(fragment_end);
                push_fragment(&mut out, base_style, &line[cursor..end]);
                cursor = end;
                continue;
            }

            let end = span.end.min(fragment_end);
            let overlay_style = if can_apply_semantic_style(base_style, styles) {
                span.kind.style(styles)
            } else {
                base_style
            };
            push_fragment(&mut out, overlay_style, &line[cursor..end]);
            cursor = end;
        }
    }

    out
}

fn can_apply_semantic_style(base_style: Style, styles: ThemeStyles) -> bool {
    base_style == Style::new() || base_style == styles.undefined_symbol
}

fn push_fragment(out: &mut StyledText, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }

    if let Some((last_style, last_text)) = out.buffer.last_mut()
        && *last_style == style
    {
        last_text.push_str(text);
        return;
    }

    out.push((style, text.to_string()));
}

#[cfg(test)]
pub(crate) fn debug_tree(line: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser.set_language(&language()).ok()?;
    Some(parser.parse(line, None)?.root_node().to_sexp())
}
