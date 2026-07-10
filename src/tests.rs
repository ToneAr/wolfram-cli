use crate::{
    commands::*, completion::*, editor::*, highlighter::*, theme::*, wl::*, wolfram_syntax::*,
};
use anyhow::Result;
use crossterm::event::{Event, KeyEvent};
use reedline::{
    Completer, EditCommand, EditMode, KeyCode, KeyModifiers, ReedlineEvent, ReedlineRawEvent,
    ValidationResult, Validator,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicU64},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn test_styles() -> ThemeStyles {
    Theme::dark().styles()
}

fn test_theme_registry() -> ThemeRegistry {
    ThemeRegistry::builtin_only()
}

fn test_user_symbols() -> Arc<Mutex<HashSet<String>>> {
    Arc::new(Mutex::new(HashSet::new()))
}

/// A `KernelBackend` that never touches a real kernel, for testing
/// completion logic without a Wolfram installation. `delay` lets a test
/// simulate a slow/blocking kernel to prove callers don't wait on it.
struct FakeBackend {
    symbols: HashMap<String, Vec<CompletionItem>>,
    details: HashMap<String, CompletionItemDetails>,
    delay: Duration,
    details_delay: Duration,
}

impl FakeBackend {
    fn empty() -> Self {
        Self {
            symbols: HashMap::new(),
            details: HashMap::new(),
            delay: Duration::ZERO,
            details_delay: Duration::ZERO,
        }
    }
}

impl KernelBackend for FakeBackend {
    fn load_symbols_for_prefix(&self, prefix: &str) -> Result<Vec<CompletionItem>> {
        thread::sleep(self.delay);
        Ok(self.symbols.get(prefix).cloned().unwrap_or_default())
    }

    fn load_symbol_details(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, CompletionItemDetails>> {
        thread::sleep(self.details_delay);
        Ok(symbols
            .iter()
            .filter_map(|symbol| {
                self.details
                    .get(symbol)
                    .cloned()
                    .map(|details| (symbol.clone(), details))
            })
            .collect())
    }

    fn load_options(&self, _head: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Polls `condition` until it's true, for waiting on the background
/// completion worker without a fixed sleep. Panics after 2s so a genuine
/// regression (worker never delivers) fails loudly instead of hanging.
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !condition() {
        if Instant::now() > deadline {
            panic!("timed out waiting for background completion result");
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn temp_completion_dir() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for attempt in 0..100 {
        let dir = std::env::temp_dir().join(format!(
            "wolfish-completion-test-{}-{unique}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => panic!("failed to create temporary completion directory {dir:?}: {err}"),
        }
    }
    panic!("failed to create a unique temporary completion directory")
}

#[test]
fn detects_options_after_first_argument_separator() {
    let line = "Plot[x, {x, 0, 1}, PlotR";
    let token_start = symbol_start(line, line.len());
    assert_eq!(option_context(line, token_start), Some("Plot".to_string()));
}

#[test]
fn does_not_offer_options_in_first_argument() {
    let line = "Plot[Sin";
    assert_eq!(option_context(line, line.len()), None);
}

#[test]
fn ignores_commas_inside_nested_argument_structures() {
    let line = "Plot[{x, x^2}";
    assert_eq!(option_context(line, line.len()), None);
}

#[test]
fn escapes_wolfram_string_literals() {
    assert_eq!(wolfram_string_literal("a\"b\\c"), "\"a\\\"b\\\\c\"");
}

#[test]
fn invokes_wolfram_files_as_immediate_function_calls() {
    let query = symbol_completion_query("LP");
    let compact_query = query.split_whitespace().collect::<String>();

    assert!(compact_query.starts_with("(Function[{p},"));
    assert!(compact_query.ends_with(")[\"LP\"]"));
    assert!(!query.contains("__PREFIX__"));
}

#[test]
fn symbol_completion_query_loads_candidates_for_fuzzy_matching() {
    let query = symbol_completion_query("LP");
    let compact_query = query.split_whitespace().collect::<String>();
    assert!(compact_query.contains("Names[StringJoin[\"*`\",p,\"*\"]]"));
    assert!(compact_query.contains("StringJoin[p,\"*\"]"));
    assert!(compact_query.contains("DeleteDuplicates[Join["));
    assert!(query.contains("Contexts[]"));
    assert!(query.contains("matchingContexts"));
    assert!(query.contains("StringStartsQ[#, p]"));
    assert!(query.contains("isPrivateContext"));
    assert!(query.contains("contextOf[name_]"));
    assert!(!query.contains("ToExpression"));
    assert!(!query.contains("WolframLanguageData"));
    // Usage messages are the expensive part of a completion query (many
    // symbols can match a short prefix); this query must stay name+context
    // only so it stays fast, and fetch usage separately in small batches.
    assert!(!query.contains("MessageName"));
}

#[test]
fn symbol_completion_reuses_broader_query_prefixes() {
    assert_eq!(symbol_query_prefix("M"), "M");
    assert_eq!(symbol_query_prefix("MyC"), "My");
    assert_eq!(symbol_query_prefix("MyContext`"), "MyContext`");
    assert_eq!(symbol_query_prefix("MyContext`foo"), "MyContext`");
}

#[test]
fn builtin_system_symbol_completion_includes_core_symbols() {
    let symbols: Vec<_> = builtin_symbols_for_prefix("System`Plo")
        .into_iter()
        .map(|item| item.value)
        .collect();

    assert!(symbols.contains(&"System`Plot".to_string()));
    assert!(symbols.contains(&"System`PlotRange".to_string()));
}

#[test]
fn symbol_details_batch_query_loads_context_and_usage_for_explicit_symbols() {
    let query = symbol_details_batch_query(&["Plot".to_string(), "Sin".to_string()]);
    let compact_query = query.split_whitespace().collect::<String>();

    assert!(compact_query.starts_with("(Function[{names},"));
    assert!(compact_query.ends_with(")[{\"Plot\",\"Sin\"}]"));
    assert!(compact_query.contains("ToExpression[StringJoin[name,\"::usage\"]]"));
    assert!(query.contains("If[StringQ[raw]"));
    assert!(query.contains("contextOf[name_]"));
    assert!(!query.contains("Symbol[name]"));
    assert!(!query.contains("Unevaluated[MessageName"));
}

#[test]
fn user_input_evaluation_preserves_kernel_context_state() {
    let expr = wolfram_user_input_evaluation_expr("x");
    let compact_expr = expr.split_whitespace().collect::<String>();

    assert!(compact_expr.starts_with("(Function[{input},"));
    assert!(expr.contains("ToExpression[input, InputForm, HoldComplete]"));
    assert!(compact_expr.ends_with(")[\"x\"]"));
    assert!(!expr.contains("$Context ="));
    assert!(!expr.contains("$ContextPath ="));
}

#[test]
fn parses_symbol_details_batch_response() {
    let details = parse_symbol_details_batch(vec![
        "Plot\tSystem`\tPlot[f, {x, xmin, xmax}] plots f.".to_string(),
        "LightCyan\tSystem`\t".to_string(),
    ]);
    assert_eq!(
        details.get("Plot"),
        Some(&CompletionItemDetails {
            context: Some("System`".to_string()),
            usage: Some("Plot[f, {x, xmin, xmax}] plots f.".to_string()),
        })
    );
    assert_eq!(
        details.get("LightCyan"),
        Some(&CompletionItemDetails {
            context: Some("System`".to_string()),
            usage: None,
        })
    );
}

#[test]
fn detects_incomplete_wolfram_input() {
    assert!(wolfram_input_is_incomplete("Plot[Sin[x]"));
    assert!(wolfram_input_is_incomplete("{1, 2"));
    assert!(wolfram_input_is_incomplete("f[(1 + 2)"));
    assert!(wolfram_input_is_incomplete("\"unterminated"));
    assert!(wolfram_input_is_incomplete("1 + (* comment"));
    assert!(wolfram_input_is_incomplete("1 + (* outer (* inner *)"));
}

#[test]
fn detects_complete_wolfram_input() {
    assert!(!wolfram_input_is_incomplete("Plot[Sin[x], {x, 0, 1}]"));
    assert!(!wolfram_input_is_incomplete("{1, 2}"));
    assert!(!wolfram_input_is_incomplete("\"[not bracket syntax]\""));
    assert!(!wolfram_input_is_incomplete("1 + (* [ignored] *) 2"));
    assert!(!wolfram_input_is_incomplete("1 + )"));
}

#[test]
fn validates_colon_commands_as_complete_input() {
    let validator = WolframValidator;
    assert!(matches!(
        validator.validate(":help ["),
        ValidationResult::Complete
    ));
}

#[test]
fn tab_key_opens_or_accepts_completion_without_submitting() {
    for (code, modifiers) in [
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::Char('i'), KeyModifiers::CONTROL),
        (KeyCode::Char('\t'), KeyModifiers::NONE),
    ] {
        let mut edit_mode = completion_edit_mode();
        let event = raw_key(code, modifiers);

        assert_eq!(
            edit_mode.parse_event(event),
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion_menu".to_string()),
                ReedlineEvent::Enter,
            ])
        );
    }
}

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let mut edit_mode = completion_edit_mode();
    let event = raw_key(KeyCode::Enter, KeyModifiers::SHIFT);

    assert_eq!(
        edit_mode.parse_event(event),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        ])
    );
}

#[test]
fn shift_tab_inserts_literal_tab_character() {
    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        let mut edit_mode = completion_edit_mode();
        let event = raw_key(KeyCode::BackTab, modifiers);

        assert_eq!(
            edit_mode.parse_event(event),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('\t')])
        );
    }
}

fn raw_key(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
    ReedlineRawEvent::convert_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
}

#[test]
fn parses_repl_commands() {
    let registry = test_theme_registry();
    assert_eq!(
        parse_repl_command(":clear", &registry).unwrap(),
        ReplCommand::Clear
    );
    assert_eq!(
        parse_repl_command(":cls", &registry).unwrap(),
        ReplCommand::Clear
    );
    assert_eq!(
        parse_repl_command(":help", &registry).unwrap(),
        ReplCommand::Help
    );
    assert_eq!(
        parse_repl_command(":?", &registry).unwrap(),
        ReplCommand::Help
    );
    assert_eq!(
        parse_repl_command(":theme", &registry).unwrap(),
        ReplCommand::Theme(ThemeCommand::Cycle)
    );
    assert_eq!(
        parse_repl_command(":theme show", &registry).unwrap(),
        ReplCommand::Theme(ThemeCommand::Show)
    );
    assert_eq!(
        parse_repl_command(":theme light", &registry).unwrap(),
        ReplCommand::Theme(ThemeCommand::Set(Theme::builtin(BuiltinTheme::Light)))
    );
    assert_eq!(
        parse_repl_command(":theme DARK", &registry).unwrap(),
        ReplCommand::Theme(ThemeCommand::Set(Theme::builtin(BuiltinTheme::Dark)))
    );
    assert_eq!(
        parse_repl_command(":theme no-color", &registry).unwrap(),
        ReplCommand::Theme(ThemeCommand::Set(Theme::plain()))
    );
    assert_eq!(
        parse_repl_command(":q", &registry).unwrap(),
        ReplCommand::Quit
    );
    assert_eq!(
        parse_repl_command(":history", &registry).unwrap(),
        ReplCommand::History
    );
    assert_eq!(
        parse_repl_command(":hist", &registry).unwrap(),
        ReplCommand::History
    );
}

#[test]
fn rejects_unknown_or_malformed_repl_commands() {
    let registry = test_theme_registry();
    assert!(parse_repl_command(":unknown", &registry).is_err());
    assert!(parse_repl_command(":clear now", &registry).is_err());
    assert!(parse_repl_command(":theme neon", &registry).is_err());
    assert!(parse_repl_command(":quit now", &registry).is_err());
    assert!(parse_repl_command(":history now", &registry).is_err());
}

#[test]
fn no_color_mode_keeps_theme_plain() {
    let theme = ThemeHandle::builtin(Theme::plain());

    assert_eq!(
        execute_repl_command(":theme dark", &theme, false),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::plain());

    assert_eq!(
        execute_repl_command(":theme", &theme, false),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::plain());
}

#[test]
fn theme_commands_can_change_theme_when_color_is_enabled() {
    let theme = ThemeHandle::builtin(Theme::plain());

    assert_eq!(
        execute_repl_command(":theme dark", &theme, true),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::dark());
}

#[test]
fn completes_repl_command_names_only_at_line_start() {
    let registry = test_theme_registry();
    let suggestions = command_completion_suggestions(":t", 2, test_styles(), &registry).unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "theme");
    assert_eq!(suggestions[0].span.start, 1);
    assert_eq!(suggestions[0].span.end, 2);

    assert!(command_completion_suggestions("x:t", 3, test_styles(), &registry).is_none());
}

#[test]
fn completes_theme_command_arguments() {
    let registry = test_theme_registry();
    let suggestions =
        command_completion_suggestions(":theme l", 8, test_styles(), &registry).unwrap();
    let matching_values: Vec<_> = suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect();
    assert_eq!(matching_values, vec!["light", "list", "ls"]);
    assert_eq!(suggestions[0].span.start, 7);
    assert_eq!(suggestions[0].span.end, 8);

    let values: Vec<_> = command_completion_suggestions(":theme ", 7, test_styles(), &registry)
        .unwrap()
        .into_iter()
        .map(|suggestion| suggestion.value)
        .collect();
    assert_eq!(
        values,
        vec![
            "dark",
            "light",
            "solarized",
            "gruvbox",
            "monokai",
            "plain",
            "list",
            "show"
        ]
    );
}

#[test]
fn colon_lines_do_not_use_wolfram_symbol_completion() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    assert!(completer.complete(":P", 2).is_empty());
}

#[test]
fn detects_cursor_inside_wolfram_strings() {
    assert!(cursor_is_in_wolfram_string("\"Pl", 3));
    assert!(cursor_is_in_wolfram_string("\"escaped \\\"Pl", 13));
    assert!(!cursor_is_in_wolfram_string("\"closed\" Pl", 11));
    assert!(!cursor_is_in_wolfram_string(
        "(* \"comment quote\" *) Pl",
        22
    ));
}

#[test]
fn filesystem_completion_is_enabled_only_for_path_like_strings() {
    let root = temp_completion_dir();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("sample.wls"), "").unwrap();
    fs::write(root.join("scratch.txt"), "").unwrap();

    let line = "Import[\"./s";
    let suggestions =
        file_completion_suggestions_from(line, line.len(), &root, None, test_styles());
    let values = suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(values, vec!["./src/", "./sample.wls", "./scratch.txt"]);
    assert_eq!(
        suggestions[0].style,
        Some(test_styles().completion_directory)
    );
    assert_eq!(suggestions[1].style, Some(test_styles().completion_file));
    assert_ne!(suggestions[0].style, suggestions[1].style);
    assert!(
        suggestions
            .iter()
            .all(|suggestion| suggestion.span.start == 8)
    );
    assert!(
        suggestions
            .iter()
            .all(|suggestion| suggestion.span.end == line.len())
    );
    assert!(!completion_is_disabled_at_cursor(line, line.len()));

    let ordinary_string = "\"sample";
    assert!(
        file_completion_suggestions_from(
            ordinary_string,
            ordinary_string.len(),
            &root,
            None,
            test_styles(),
        )
        .is_empty()
    );
    assert!(completion_is_disabled_at_cursor(
        ordinary_string,
        ordinary_string.len()
    ));

    let outside_string = "./s";
    assert!(
        file_completion_suggestions_from(
            outside_string,
            outside_string.len(),
            &root,
            None,
            test_styles(),
        )
        .is_empty()
    );
}

#[test]
fn filesystem_completion_expands_home_paths_inside_strings() {
    let root = temp_completion_dir();
    fs::create_dir(root.join("Documents")).unwrap();
    fs::write(root.join("Downloads.txt"), "").unwrap();

    let line = "File[\"~/Do";
    let suggestions = file_completion_suggestions_from(
        line,
        line.len(),
        &temp_completion_dir(),
        Some(&root),
        test_styles(),
    );
    let values = suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(values, vec!["~/Documents/", "~/Downloads.txt"]);
}

#[test]
fn symbol_completion_is_disabled_inside_strings() {
    let user_symbols = test_user_symbols();
    remember_user_symbols("PlotMine = 1", &user_symbols);
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols,
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let inside_string = "StringJoin[\"Pl";
    let escaped_quote_string = "\"escaped \\\"Pl";
    let after_string = "\"closed\" Pl";

    assert!(
        completer
            .complete(inside_string, inside_string.len())
            .is_empty()
    );
    assert!(
        completer
            .complete(escaped_quote_string, escaped_quote_string.len())
            .is_empty()
    );
    assert!(
        completer
            .complete(after_string, after_string.len())
            .iter()
            .any(|suggestion| suggestion.value == "PlotMine")
    );
}

#[test]
fn parses_symbol_and_context_completion_items() {
    assert_eq!(
        parse_completion_items(vec![
            "symbol\tfooBar\t42\tMyCtx`".to_string(),
            "context\tMyCtx`\t0\tMyCtx`".to_string(),
            "ignored\tvalue".to_string(),
            "malformed".to_string(),
        ]),
        vec![
            CompletionItem {
                value: "fooBar".to_string(),
                kind: CompletionKind::Symbol,
                frequency: Some(42),
                context: Some("MyCtx`".to_string()),
            },
            CompletionItem {
                value: "MyCtx`".to_string(),
                kind: CompletionKind::Context,
                frequency: Some(0),
                context: Some("MyCtx`".to_string()),
            },
        ]
    );
}

#[test]
fn broad_symbol_sets_do_not_claim_usage_details() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let symbols: Vec<_> = (0..=USAGE_DETAIL_MAX_MATCHES)
        .map(|idx| CompletionItem {
            value: format!("BroadMatch{idx}"),
            kind: CompletionKind::Symbol,
            frequency: None,
            context: Some("System`".to_string()),
        })
        .collect();

    let suggestions = symbol_suggestions(&symbols, "Broad", 0, 5, &source, test_styles());

    assert_eq!(suggestions.len(), symbols.len());
    assert!(
        source
            .details_cache
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty(),
        "broad completion sets should not enqueue usage lookups while the menu is open"
    );
}

#[test]
fn narrow_symbol_sets_claim_only_visible_usage_details() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let symbols: Vec<_> = (0..20)
        .map(|idx| CompletionItem {
            value: format!("NarrowMatch{idx}"),
            kind: CompletionKind::Symbol,
            frequency: None,
            context: Some("System`".to_string()),
        })
        .collect();

    let suggestions = symbol_suggestions(&symbols, "Narrow", 0, 6, &source, test_styles());

    assert_eq!(suggestions.len(), symbols.len());
    assert_eq!(
        source
            .details_cache
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len(),
        USAGE_LOOKAHEAD
    );
}

#[test]
fn reattaches_context_when_names_returns_short_symbol() {
    let backend = FakeBackend {
        symbols: HashMap::new(),
        details: HashMap::from([(
            "System`Plot".to_string(),
            CompletionItemDetails {
                context: Some("System`".to_string()),
                usage: Some("Plot[f, {x, xmin, xmax}] plots f.".to_string()),
            },
        )]),
        delay: Duration::ZERO,
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let candidate = CompletionItem {
        value: "Plot".to_string(),
        kind: CompletionKind::Symbol,
        frequency: None,
        context: Some("System`".to_string()),
    };

    // The first call only spawns the background detail fetch; wait for the
    // worker to deliver it before checking that it gets picked up.
    symbol_suggestions(
        std::slice::from_ref(&candidate),
        "System`P",
        0,
        8,
        &source,
        test_styles(),
    );
    wait_until(|| {
        !source
            .usage_details(&["System`Plot".to_string()])
            .is_empty()
    });

    let suggestions = symbol_suggestions(&[candidate], "System`P", 0, 8, &source, test_styles());

    assert_eq!(suggestions[0].value, "System`Plot");
    assert_eq!(
        suggestions[0].description.as_deref(),
        Some("symbol\nContext: System`\nUsage: Plot[f, {x, xmin, xmax}] plots f.")
    );
}

#[test]
fn completion_menu_colors_symbols_contexts_and_global_symbols_differently() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let builtin = CompletionItem {
        value: "Plot".to_string(),
        kind: CompletionKind::Symbol,
        frequency: None,
        context: Some("System`".to_string()),
    };
    let global = CompletionItem {
        value: "myGlobal".to_string(),
        kind: CompletionKind::Symbol,
        frequency: None,
        context: Some("Global`".to_string()),
    };
    let user_defined = CompletionItem {
        value: "myVar".to_string(),
        kind: CompletionKind::Symbol,
        frequency: None,
        context: Some("MyContext`".to_string()),
    };
    let context = CompletionItem {
        value: "MyContext`".to_string(),
        kind: CompletionKind::Context,
        frequency: None,
        context: Some("MyContext`".to_string()),
    };

    let suggestions = symbol_suggestions(
        &[builtin, global, user_defined, context],
        "",
        0,
        0,
        &source,
        test_styles(),
    );

    let builtin_style = suggestions
        .iter()
        .find(|s| s.value == "Plot")
        .unwrap()
        .style
        .unwrap();
    let global_style = suggestions
        .iter()
        .find(|s| s.value == "myGlobal")
        .unwrap()
        .style
        .unwrap();
    let user_style = suggestions
        .iter()
        .find(|s| s.value == "myVar")
        .unwrap()
        .style
        .unwrap();
    let context_style = suggestions
        .iter()
        .find(|s| s.value == "MyContext`")
        .unwrap()
        .style
        .unwrap();

    assert_eq!(builtin_style, test_styles().completion_symbol);
    assert_eq!(global_style, test_styles().completion_global_symbol);
    assert_eq!(user_style, test_styles().completion_user_symbol);
    assert_eq!(context_style, test_styles().completion_context);
    assert_ne!(builtin_style, global_style);
    assert_ne!(global_style, user_style);
    assert_ne!(user_style, context_style);
}

#[test]
fn fuzzy_matches_completion_candidates() {
    assert!(fuzzy_matches("PlotRange", "PlR"));
    assert!(fuzzy_matches("PlotRange", "plotr"));
    assert!(fuzzy_matches("ListPlot", "LP"));
    assert!(!fuzzy_matches("ListContourPlot", "LP"));
    assert!(!fuzzy_matches("PlotRange", "Prl"));
}

#[test]
fn weighs_completion_scores_by_builtin_frequency() {
    assert!(
        completion_score("PopularMatch", "Pop", Some(80))
            < completion_score("PrefixMatch", "Pre", Some(0))
    );
}

#[test]
fn context_suggestions_match_qualified_prefixes() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let suggestions = symbol_suggestions(
        &[CompletionItem {
            value: "MyPackage`".to_string(),
            kind: CompletionKind::Context,
            frequency: None,
            context: Some("MyPackage`".to_string()),
        }],
        "MyP",
        0,
        3,
        &source,
        test_styles(),
    );

    assert_eq!(suggestions[0].value, "MyPackage`");
}

#[test]
fn ranks_user_symbols_before_frequent_builtins() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut suggestions = symbol_suggestions(
        &[
            CompletionItem {
                value: "Plot".to_string(),
                kind: CompletionKind::Symbol,
                frequency: Some(99),
                context: Some("System`".to_string()),
            },
            CompletionItem {
                value: "PlotMine".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("Global`".to_string()),
            },
        ],
        "Plot",
        0,
        4,
        &source,
        test_styles(),
    );
    suggestions.sort_by(|left, right| {
        completion_sort_key(left, "Plot")
            .cmp(&completion_sort_key(right, "Plot"))
            .then_with(|| left.value.cmp(&right.value))
    });

    assert_eq!(suggestions[0].value, "PlotMine");
}

#[test]
fn local_assigned_symbols_complete_immediately() {
    let user_symbols = test_user_symbols();
    remember_user_symbols("var = 10", &user_symbols);
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols,
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let suggestions = completer.complete("var", 3);

    assert_eq!(suggestions[0].value, "var");
    assert_eq!(
        suggestions[0].description.as_deref(),
        Some("symbol\nContext: Global`")
    );
}

#[test]
fn local_beginpackage_contexts_complete_immediately() {
    let user_symbols = test_user_symbols();
    remember_user_symbols("BeginPackage[\"MyContext`\"]", &user_symbols);
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols,
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let suggestions = completer.complete("MyC", 3);

    assert_eq!(suggestions[0].value, "MyContext`");
    assert_eq!(
        suggestions[0].description.as_deref(),
        Some("context\nContext: MyContext`")
    );
}

#[test]
fn kernel_contexts_complete_for_unqualified_prefixes() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "My".to_string(),
            vec![CompletionItem {
                value: "MyContext`".to_string(),
                kind: CompletionKind::Context,
                frequency: None,
                context: Some("MyContext`".to_string()),
            }],
        )]),
        details: HashMap::new(),
        delay: Duration::ZERO,
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let _ = completer.complete("MyC", 3);
    wait_until(|| {
        completer
            .source
            .symbols_for_prefix("MyC")
            .iter()
            .any(|item| item.value == "MyContext`")
    });

    let suggestions = completer.complete("MyC", 3);

    assert!(
        suggestions
            .iter()
            .any(|suggestion| suggestion.value == "MyContext`")
    );
}

#[test]
fn kernel_symbols_inside_contexts_complete_after_context_prefix() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "MyContext`".to_string(),
            vec![CompletionItem {
                value: "foo".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("MyContext`".to_string()),
            }],
        )]),
        details: HashMap::new(),
        delay: Duration::ZERO,
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let _ = completer.complete("MyContext`", 10);
    wait_until(|| {
        completer
            .source
            .symbols_for_prefix("MyContext`")
            .iter()
            .any(|item| item.value == "foo")
    });

    let suggestions = completer.complete("MyContext`", 10);

    assert!(
        suggestions
            .iter()
            .any(|suggestion| suggestion.value == "MyContext`foo")
    );
}

#[test]
fn local_qualified_assignments_remember_context_and_symbol() {
    let user_symbols = test_user_symbols();
    remember_user_symbols("MyContext`foo = 1", &user_symbols);
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols,
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let context_suggestions = completer.complete("MyC", 3);
    let symbol_suggestions = completer.complete("foo", 3);

    assert_eq!(context_suggestions[0].value, "MyContext`");
    assert_eq!(symbol_suggestions[0].value, "foo");
}

#[test]
fn ranks_direct_matches_before_longer_fuzzy_symbol_matches() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let suggestions = symbol_suggestions(
        &[
            CompletionItem {
                value: "ListContourPlot".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: None,
            },
            CompletionItem {
                value: "ListLineIntegralConvolutionPlot".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: None,
            },
            CompletionItem {
                value: "ListPlot".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: None,
            },
        ],
        "ListPlo",
        0,
        7,
        &source,
        test_styles(),
    );

    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "ListPlot");
}

#[test]
fn fuzzy_matches_symbols_options_and_arguments() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let symbol_suggestions = symbol_suggestions(
        &[CompletionItem {
            value: "PlotRange".to_string(),
            kind: CompletionKind::Symbol,
            frequency: None,
            context: None,
        }],
        "PlR",
        0,
        3,
        &source,
        test_styles(),
    );
    assert_eq!(symbol_suggestions[0].value, "PlotRange");

    let option_suggestions = option_suggestions(
        &["PlotRange".to_string()],
        "PlR",
        0,
        3,
        "Plot",
        test_styles(),
    );
    assert_eq!(option_suggestions[0].value, "PlotRange");
}

#[test]
fn completion_never_blocks_on_a_slow_kernel_backend() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "Pl".to_string(),
            vec![CompletionItem {
                value: "PlotSomething".to_string(),
                kind: CompletionKind::Symbol,
                frequency: Some(0),
                context: Some("Global`".to_string()),
            }],
        )]),
        details: HashMap::new(),
        delay: Duration::from_secs(2),
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let start = Instant::now();
    let _ = completer.complete("Pl", 2);
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(200),
        "complete() blocked for {elapsed:?}; kernel queries must run on the \
         background worker, never on reedline's input thread"
    );
}

#[test]
fn async_cache_poll_does_not_block_when_locked() {
    let cache: AsyncCache<String, i32> = AsyncCache::new();
    let _guard = cache
        .entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let start = Instant::now();
    let result = cache.poll_or_claim(&"a".to_string(), 0);
    let elapsed = start.elapsed();

    assert!(matches!(result, CachePoll::Pending));
    assert!(
        elapsed < Duration::from_millis(50),
        "cache poll blocked for {elapsed:?}; completion cache reads must be non-blocking"
    );
}

#[test]
fn slow_usage_details_do_not_starve_symbol_completion() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "Private`".to_string(),
            vec![CompletionItem {
                value: "yyFast".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("Global`".to_string()),
            }],
        )]),
        details: HashMap::new(),
        delay: Duration::ZERO,
        details_delay: Duration::from_secs(2),
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );

    assert!(source.usage_details(&["SlowUsage".to_string()]).is_empty());
    assert!(source.symbols_for_prefix("Private`yy").is_empty());

    wait_until(|| {
        source
            .symbols_for_prefix("Private`yy")
            .iter()
            .any(|item| item.value == "yyFast")
    });
}

#[test]
fn async_cache_treats_stale_epoch_entries_as_missing() {
    let cache: AsyncCache<String, i32> = AsyncCache::new();

    // Nothing cached yet: caller is told to spawn a fetch, and claims the
    // key so a second concurrent caller doesn't spawn a duplicate.
    assert!(matches!(
        cache.poll_or_claim(&"a".to_string(), 0),
        CachePoll::Spawn
    ));
    assert!(matches!(
        cache.poll_or_claim(&"a".to_string(), 0),
        CachePoll::Pending
    ));

    cache.insert("a".to_string(), 0, 42);
    match cache.poll_or_claim(&"a".to_string(), 0) {
        CachePoll::Ready(value) => assert_eq!(value, 42),
        _ => panic!("expected a Ready entry after insert"),
    }

    // A later epoch (kernel gained new definitions) makes the old entry
    // stale without needing an explicit cache-clearing pass.
    assert!(matches!(
        cache.poll_or_claim(&"a".to_string(), 1),
        CachePoll::Spawn
    ));
}

fn style_of(
    text: &str,
    builtin: &HashSet<String>,
    user: &HashSet<String>,
    word: &str,
) -> nu_ansi_term::Style {
    let styled = highlight_wolfram_text(text, test_styles(), Some(builtin), Some(user));
    styled
        .buffer
        .into_iter()
        .find(|(_, fragment)| fragment == word)
        .map(|(style, _)| style)
        .unwrap_or_else(|| panic!("word {word:?} not found in highlighted output for {text:?}"))
}

#[test]
fn highlighter_colors_builtin_symbols() {
    let builtin: HashSet<String> = ["Plot".to_string()].into_iter().collect();
    let user: HashSet<String> = HashSet::new();
    let style = style_of("Plot[x]", &builtin, &user, "Plot");
    assert_eq!(style, test_styles().builtin_symbol);
}

#[test]
fn highlighter_colors_explicit_system_context_as_builtin_even_if_unknown() {
    let builtin: HashSet<String> = HashSet::new();
    let user: HashSet<String> = HashSet::new();
    let style = style_of("System`Foo[x]", &builtin, &user, "System`Foo");
    assert_eq!(style, test_styles().builtin_symbol);
}

#[test]
fn highlighter_colors_defined_user_symbols_differently_from_builtins() {
    let builtin: HashSet<String> = ["Plot".to_string()].into_iter().collect();
    let user: HashSet<String> = ["myVar".to_string()].into_iter().collect();
    let style = style_of("myVar + Plot[x]", &builtin, &user, "myVar");
    assert_eq!(style, test_styles().user_symbol);
    assert_ne!(style, test_styles().builtin_symbol);
}

#[test]
fn highlighter_leaves_undefined_symbols_unstyled() {
    let builtin: HashSet<String> = ["Plot".to_string()].into_iter().collect();
    let user: HashSet<String> = ["myVar".to_string()].into_iter().collect();
    let style = style_of(
        "undefinedThing + Plot[x]",
        &builtin,
        &user,
        "undefinedThing",
    );
    assert_eq!(style, nu_ansi_term::Style::new());
}
