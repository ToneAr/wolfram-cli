use crate::{
    commands::*, completion::*, editor::*, highlighter::*, kernel::KernelStatus, theme::*, wl::*,
    wolfram_syntax::*,
};
use anyhow::Result;
use crossterm::event::{Event, KeyEvent};
use reedline::{
    Completer, EditCommand, EditMode, FileBackedHistory, Highlighter, Hinter, KeyCode,
    KeyModifiers, Prompt, ReedlineEvent, ReedlineRawEvent, Span, ValidationResult, Validator,
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
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

fn test_suggestion(value: &str, start: usize, end: usize) -> reedline::Suggestion {
    reedline::Suggestion {
        value: value.to_string(),
        description: None,
        style: None,
        extra: None,
        span: Span { start, end },
        append_whitespace: false,
    }
}

#[test]
fn completion_ignores_stale_cursor_position_after_private_context_completion() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));
    let line = "WAssistant`Server`Source`Jobs`Private`twikiFindNewRouteroot";

    assert!(completer.complete(line, line.len() + 5).is_empty());
}

fn temp_completion_dir() -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for attempt in 0..100 {
        let dir = std::env::temp_dir().join(format!(
            "wolfie-completion-test-{}-{unique}-{attempt}",
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
fn completion_hint_suffix_uses_only_untyped_remainder() {
    let line = "PlotR";

    assert_eq!(
        completion_hint_suffix(
            line,
            line.len(),
            &test_suggestion("PlotRange", 0, line.len())
        ),
        Some("ange".to_string())
    );
    assert_eq!(
        completion_hint_suffix(line, 3, &test_suggestion("PlotRange", 0, line.len())),
        None
    );
    assert_eq!(
        completion_hint_suffix(line, line.len(), &test_suggestion("Range", 0, line.len())),
        None
    );
}

#[test]
fn down_cycles_ghost_completion_when_menu_is_disabled() {
    let user_symbols = test_user_symbols();
    {
        let mut symbols = user_symbols
            .lock()
            .expect("user symbols lock should be available");
        symbols.insert("zzAlpha".to_string());
        symbols.insert("zzBeta".to_string());
    }
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols,
    );
    let selection = GhostCompletionSelection::new();
    let mut hinter = WolframCompletionHinter::new(
        source,
        ThemeHandle::builtin(Theme::dark()),
        selection.clone(),
    );
    let history = FileBackedHistory::default();

    assert_eq!(hinter.handle("zz", 2, &history, false), "Alpha");

    let mut edit_mode = history_primed_edit_mode(
        completion_edit_mode(false),
        HistoryTrigger::new(),
        Some(selection),
    );
    assert_eq!(
        edit_mode.parse_event(raw_key(KeyCode::Down, KeyModifiers::NONE)),
        ReedlineEvent::Repaint
    );
    assert_eq!(hinter.handle("zz", 2, &history, false), "Beta");
}

#[test]
fn parses_symbol_details_batch() {
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
fn script_source_evaluation_suppresses_null_results() {
    let compact_source = EVALUATE_SCRIPT_SOURCE_WL
        .split_whitespace()
        .collect::<String>();

    assert!(compact_source.contains("If[result===Null,\"\",result]"));
}

#[test]
fn symbol_completion_query_loads_candidates_for_fuzzy_matching() {
    let query = symbol_completion_query("LP");
    let compact_query = query.split_whitespace().collect::<String>();
    assert!(compact_query.contains("currentContext=$Context"));
    assert!(compact_query.contains("Names[StringJoin[currentContext,p,\"*\"]]"));
    assert!(compact_query.contains("Names[StringJoin[p,\"*\"]]"));
    assert!(compact_query.contains("Names[StringJoin[\"*`\",p,\"*\"]]"));
    assert!(compact_query.contains("item[#,currentContext]&/@"));
    assert!(compact_query.contains("requestedContext="));
    assert!(compact_query.contains("item[#,requestedContext]&/@"));
    assert!(compact_query.contains("DeleteDuplicates[items]"));
    assert!(query.contains("Contexts[]"));
    assert!(query.contains("matchingContexts"));
    assert!(query.contains("StringStartsQ[#, p]"));
    assert!(query.contains("isPrivateContext"));
    assert!(query.contains("showsPrivateContext"));
    assert!(
        compact_query.contains("showsPrivateContext=(isPrivateContext[#]&&StringStartsQ[p,#])&;")
    );
    assert!(query.contains("isVisibleContext"));
    assert!(
        compact_query.contains("isVisibleContext=(!isPrivateContext[#]||showsPrivateContext[#])&;")
    );
    assert!(query.contains("contextOf"));
    assert!(query.contains("StringReplace[#1"));
    assert!(!query.contains("ToExpression"));
    assert!(!query.contains("WolframLanguageData"));
    // Usage messages are the expensive part of a completion query (many
    // symbols can match a short prefix); this query must stay name+context
    // only so it stays fast, and fetch usage separately in small batches.
    assert!(!query.contains("MessageName"));
}

#[test]
fn finds_contexts_loaded_with_get() {
    assert_eq!(
        loaded_context_names("<<DatabaseLink`"),
        vec!["DatabaseLink`".to_string()]
    );
    assert_eq!(
        loaded_context_names("<<DatabaseLink`; << OtherPackage`"),
        vec!["DatabaseLink`".to_string(), "OtherPackage`".to_string()]
    );
    assert!(loaded_context_names("<< \"package.wl\"").is_empty());
}

#[test]
fn symbol_completion_reuses_broader_query_prefixes() {
    assert_eq!(symbol_query_prefix("M"), "M");
    assert_eq!(symbol_query_prefix("MyC"), "My");
    assert_eq!(symbol_query_prefix("MyContext`"), "MyContext`");
    assert_eq!(symbol_query_prefix("MyContext`foo"), "MyContext`");
}

#[test]
fn symbol_highlighter_uses_exact_names_lookup() {
    let query = symbol_definition_query("xy");
    let compact_query = query.split_whitespace().collect::<String>();

    assert!(compact_query.starts_with("(Function[{name},"));
    assert!(compact_query.contains("Names[name]==={}"));
    assert!(compact_query.ends_with(")[\"xy\"]"));
    assert!(!query.contains('*'));
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
fn tab_key_accepts_hint_before_opening_completion_menu() {
    for (code, modifiers) in [
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::Char('i'), KeyModifiers::CONTROL),
        (KeyCode::Char('\t'), KeyModifiers::NONE),
    ] {
        let mut edit_mode = completion_edit_mode(true);
        let event = raw_key(code, modifiers);

        assert_eq!(
            edit_mode.parse_event(event),
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::HistoryHintComplete,
                ReedlineEvent::Menu("completion_menu".to_string()),
                ReedlineEvent::Enter,
            ])
        );
    }
}

#[test]
fn shift_enter_inserts_newline_without_submitting() {
    let mut edit_mode = completion_edit_mode(true);
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
        let mut edit_mode = completion_edit_mode(true);
        let event = raw_key(KeyCode::BackTab, modifiers);

        assert_eq!(
            edit_mode.parse_event(event),
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('\t')])
        );
    }
}

#[test]
fn colon_key_opens_command_completion_menu() {
    for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
        let mut edit_mode = completion_edit_mode(true);
        let event = raw_key(KeyCode::Char(':'), modifiers);

        assert_eq!(
            edit_mode.parse_event(event),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(':')]),
                ReedlineEvent::Menu("completion_menu".to_string()),
            ])
        );
    }
}

#[test]
fn disabled_completion_menu_does_not_open_popup_bindings() {
    let mut edit_mode = completion_edit_mode(false);

    assert_eq!(
        edit_mode.parse_event(raw_key(KeyCode::Char('P'), KeyModifiers::SHIFT)),
        ReedlineEvent::Edit(vec![EditCommand::InsertChar('P')])
    );
    assert_eq!(
        edit_mode.parse_event(raw_key(KeyCode::Char(':'), KeyModifiers::NONE)),
        ReedlineEvent::Edit(vec![EditCommand::InsertChar(':')])
    );
    assert_eq!(
        edit_mode.parse_event(raw_key(KeyCode::Tab, KeyModifiers::NONE)),
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('\t')]),
        ])
    );
}

#[test]
fn paste_inserts_text_in_one_edit_without_opening_completion() {
    let mut edit_mode =
        history_primed_edit_mode(completion_edit_mode(true), HistoryTrigger::new(), None);
    let event = raw_paste("Plot[Sin[x], {x, 0, 1}]\r\nN[%]");

    assert_eq!(
        edit_mode.parse_event(event),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![EditCommand::InsertString(
                "Plot[Sin[x], {x, 0, 1}]\nN[%]".to_string()
            )]),
        ])
    );
}

#[test]
fn oversized_paste_is_inserted_once_and_editor_paths_accept_it() {
    let input = "x".repeat(usize::from(u16::MAX) + 1);
    let mut edit_mode =
        history_primed_edit_mode(completion_edit_mode(true), HistoryTrigger::new(), None);

    assert_eq!(
        edit_mode.parse_event(raw_paste(&input)),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![EditCommand::InsertString(input.clone())]),
        ])
    );

    let highlighter = WolframHighlighter::new(
        builtin_symbol_set(),
        test_user_symbols(),
        None,
        None,
        ThemeHandle::builtin(Theme::dark()),
        Arc::new(AtomicBool::new(false)),
    );
    let highlighted = highlighter.highlight(&input, input.len());
    assert_eq!(highlighted.buffer.len(), 1);

    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));
    assert!(completer.complete(&input, input.len()).is_empty());
}

fn raw_key(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
    ReedlineRawEvent::convert_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
}

fn raw_paste(body: &str) -> ReedlineRawEvent {
    ReedlineRawEvent::convert_from(Event::Paste(body.to_string())).unwrap()
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
        parse_repl_command(":config", &registry).unwrap(),
        ReplCommand::Settings
    );
    assert_eq!(
        parse_repl_command(":conf", &registry).unwrap(),
        ReplCommand::Settings
    );
    assert_eq!(
        parse_repl_command(":setting", &registry).unwrap(),
        ReplCommand::Settings
    );
    assert_eq!(
        parse_repl_command(":settings", &registry).unwrap(),
        ReplCommand::Settings
    );
    assert_eq!(
        parse_repl_command(":config show", &registry).unwrap(),
        ReplCommand::Config(ConfigCommand::Show)
    );
    assert_eq!(
        parse_repl_command(":config edit", &registry).unwrap(),
        ReplCommand::Config(ConfigCommand::Edit)
    );
    assert_eq!(
        parse_repl_command(":conf edit", &registry).unwrap(),
        ReplCommand::Config(ConfigCommand::Edit)
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
    assert!(parse_repl_command(":setting now", &registry).is_err());
    assert!(parse_repl_command(":config show now", &registry).is_err());
    assert!(parse_repl_command(":config edit now", &registry).is_err());
}

#[test]
fn no_color_mode_keeps_theme_plain() {
    let theme = ThemeHandle::builtin(Theme::plain());

    assert_eq!(
        execute_repl_command(":theme dark", &theme, false, ConfigMode::User),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::plain());

    assert_eq!(
        execute_repl_command(":theme", &theme, false, ConfigMode::User),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::plain());
}

#[test]
fn theme_commands_can_change_theme_when_color_is_enabled() {
    let theme = ThemeHandle::builtin(Theme::plain());

    assert_eq!(
        execute_repl_command(":theme dark", &theme, true, ConfigMode::User),
        CommandAction::Continue
    );
    assert_eq!(theme.current(), Theme::dark());
}

#[test]
fn completes_repl_command_names_only_at_line_start() {
    let registry = test_theme_registry();
    let bare_suggestions =
        command_completion_suggestions(":", 1, test_styles(), &registry).unwrap();
    let bare_values = bare_suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        bare_values,
        vec![
            "clear", "config", "conf", "help", "history", "setting", "settings", "theme", "quit"
        ]
    );
    assert!(
        bare_suggestions
            .iter()
            .all(|suggestion| suggestion.span.start == 1 && suggestion.span.end == 1)
    );

    let suggestions = command_completion_suggestions(":t", 2, test_styles(), &registry).unwrap();
    assert_eq!(suggestions.len(), 1);
    assert_eq!(suggestions[0].value, "theme");
    assert_eq!(suggestions[0].span.start, 1);
    assert_eq!(suggestions[0].span.end, 2);

    assert!(command_completion_suggestions("x:t", 3, test_styles(), &registry).is_none());
}

#[test]
fn completes_config_command_arguments() {
    let registry = test_theme_registry();
    let suggestions =
        command_completion_suggestions(":config e", 9, test_styles(), &registry).unwrap();
    let values = suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect::<Vec<_>>();
    assert_eq!(values, vec!["edit"]);
    assert_eq!(suggestions[0].span.start, 8);
    assert_eq!(suggestions[0].span.end, 9);
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
fn shell_escape_completion_suggests_path_commands_for_the_first_token() {
    let root = temp_completion_dir();
    let command = if cfg!(windows) { "git.exe" } else { "git" };
    fs::write(root.join(command), "").unwrap();
    fs::write(
        root.join(if cfg!(windows) { "gone.cmd" } else { "gone" }),
        "",
    )
    .unwrap();
    fs::write(root.join("not-a-command.txt"), "").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["git", "gone"] {
            let path = root.join(name);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    let line = ":!gi";
    let suggestions = shell_completion_suggestions_from(
        line,
        line.len(),
        &root,
        None,
        vec![root.clone()],
        Some(std::ffi::OsStr::new(".EXE;.CMD")),
        test_styles(),
    );

    assert_eq!(
        suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect::<Vec<_>>(),
        vec!["git"]
    );
    assert_eq!(
        suggestions[0].span,
        Span {
            start: 2,
            end: line.len()
        }
    );
    assert_eq!(
        suggestions[0].description.as_deref(),
        Some("command on PATH")
    );
}

#[test]
fn shell_escape_completion_uses_file_paths_for_path_like_arguments() {
    let root = temp_completion_dir();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("sample.wls"), "").unwrap();
    fs::write(root.join("scratch.txt"), "").unwrap();

    let line = ":!ls ./s";
    let suggestions =
        shell_file_completion_suggestions_from(line, line.len(), &root, None, test_styles());
    let values = suggestions
        .iter()
        .map(|suggestion| suggestion.value.as_str())
        .collect::<Vec<_>>();

    assert_eq!(values, vec!["./src/", "./sample.wls", "./scratch.txt"]);
    assert_eq!(suggestions[0].span.start, 5);
    assert_eq!(suggestions[0].span.end, line.len());
    assert_eq!(
        suggestions[0].style,
        Some(test_styles().completion_directory)
    );
    assert_eq!(suggestions[1].style, Some(test_styles().completion_file));
}

#[test]
fn shell_escape_completion_ignores_non_path_arguments() {
    let root = temp_completion_dir();
    fs::write(root.join("sample.wls"), "").unwrap();

    assert!(
        shell_file_completion_suggestions_from(":!echo sam", 10, &root, None, test_styles())
            .is_empty()
    );
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

    assert_eq!(suggestions[0].value, "Plot");
    assert_eq!(suggestions[0].span, Span { start: "System`".len(), end: 8 });
    assert_eq!(
        suggestions[0].description.as_deref(),
        Some("symbol\nContext: System`\nUsage: Plot[f, {x, xmin, xmax}] plots f.")
    );
}

#[test]
fn long_qualified_symbols_replace_only_the_final_segment() {
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let prefix = "ServiceFramework`PackageScope`deleteLocalCredentials";
    let suggestions = symbol_suggestions(
        &[CompletionItem {
            value: "deleteLocalCredentials".to_string(),
            kind: CompletionKind::Symbol,
            frequency: None,
            context: Some("ServiceFramework`PackageScope`".to_string()),
        }],
        prefix,
        0,
        prefix.len(),
        &source,
        test_styles(),
    );

    assert_eq!(suggestions[0].value, "deleteLocalCredentials");
    assert_eq!(
        suggestions[0].span,
        Span {
            start: "ServiceFramework`PackageScope`".len(),
            end: prefix.len(),
        }
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
            .symbols_for_prefix_wait("MyC", Duration::ZERO)
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
fn kernel_symbols_inside_contexts_complete_on_backtick_keystroke() {
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
        delay: Duration::from_millis(20),
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let mut completer = WolframCompleter::new(source, ThemeHandle::builtin(Theme::dark()));

    let suggestions = completer.complete("MyContext`", 10);

    assert!(suggestions.iter().any(|suggestion| {
        suggestion.value == "foo"
            && suggestion.span
                == Span {
                    start: "MyContext`".len(),
                    end: "MyContext`".len(),
                }
    }));
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
fn context_delimiter_completion_wait_is_bounded_for_slow_kernel_backend() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "SlowContext`".to_string(),
            vec![CompletionItem {
                value: "foo".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("SlowContext`".to_string()),
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
    let suggestions = completer.complete("SlowContext`", 12);
    let elapsed = start.elapsed();

    assert!(suggestions.is_empty());
    assert!(
        elapsed < CONTEXT_COMPLETION_WAIT + Duration::from_millis(150),
        "context completion wait should be bounded, but took {elapsed:?}"
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
    assert!(
        source
            .symbols_for_prefix_wait("Private`yy", Duration::ZERO)
            .is_empty()
    );

    wait_until(|| {
        source
            .symbols_for_prefix_wait("Private`yy", Duration::ZERO)
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
    style_of_with_known(text, builtin, user, None, word)
}

fn style_of_with_known(
    text: &str,
    builtin: &HashSet<String>,
    user: &HashSet<String>,
    known: Option<&HashSet<String>>,
    word: &str,
) -> nu_ansi_term::Style {
    let styled =
        highlight_wolfram_text(text, test_styles(), Some(builtin), Some(user), known, None);
    styled
        .buffer
        .into_iter()
        .find(|(_, fragment)| fragment == word)
        .map(|(style, _)| style)
        .unwrap_or_else(|| panic!("word {word:?} not found in highlighted output for {text:?}"))
}

fn highlighted_fragments(text: &str) -> Vec<(nu_ansi_term::Style, String)> {
    highlight_wolfram_text(text, test_styles(), None, None, None, None).buffer
}

fn highlighted_fragments_at_cursor(
    text: &str,
    cursor: usize,
) -> Vec<(nu_ansi_term::Style, String)> {
    highlight_wolfram_text_at_cursor(text, cursor, test_styles(), None, None, None, None).buffer
}

fn highlighted_text_at_cursor(text: &str, cursor: usize) -> String {
    highlighted_fragments_at_cursor(text, cursor)
        .into_iter()
        .map(|(_, fragment)| fragment)
        .collect()
}

#[test]
fn highlighter_colors_builtin_symbols() {
    let builtin: HashSet<String> = ["Plot".to_string()].into_iter().collect();
    let user: HashSet<String> = HashSet::new();
    let style = style_of("Plot[x]", &builtin, &user, "Plot");
    assert_eq!(style, test_styles().builtin_symbol);
}

#[test]
fn highlighter_collapses_command_mode_marker_to_cyan_colon() {
    assert_eq!(
        highlighted_fragments(":help Plot"),
        vec![
            (
                nu_ansi_term::Style::new().fg(nu_ansi_term::Color::Cyan),
                ":".to_string()
            ),
            (nu_ansi_term::Style::new(), "help Plot".to_string())
        ]
    );
}

#[test]
fn shell_escape_prompt_is_active_after_marker() {
    assert!(!shell_escape_prompt_is_active(":", 1));
    assert!(shell_escape_prompt_is_active(":!", 2));
    assert!(shell_escape_prompt_is_active(":!ls", 4));
    assert!(shell_escape_prompt_is_active("  :!ls", 6));
    assert!(!shell_escape_prompt_is_active("Print[\":!\"]", 10));
}

#[test]
fn wolfram_prompt_is_hidden_while_shell_escape_is_active() {
    let shell_prompt_hidden = Arc::new(AtomicBool::new(false));
    let prompt = WolframPrompt {
        input_prompt: "In[7]:= ".to_string(),
        kernel_status: KernelStatus::ReadyWstp,
        theme: ThemeHandle::builtin(Theme::dark()),
        show_prompt: true,
        shell_prompt_hidden: shell_prompt_hidden.clone(),
    };

    assert!(prompt.render_prompt_left().contains("In[7]:="));

    shell_prompt_hidden.store(true, Ordering::Relaxed);
    assert!(
        prompt
            .render_prompt_left()
            .contains(&format!("{} ", shell_escape_name()))
    );
    assert_eq!(prompt.render_prompt_right(), "");
    assert_eq!(prompt.render_prompt_multiline_indicator(), "");
}

#[test]
fn highlighter_runs_without_dynamic_completion_lookup() {
    let highlighter = WolframHighlighter::new(
        builtin_symbol_set(),
        test_user_symbols(),
        None,
        None,
        ThemeHandle::builtin(Theme::dark()),
        Arc::new(AtomicBool::new(false)),
    );

    let highlighted = highlighter.highlight("UnknownPackage`symbol", 21);

    assert!(!highlighted.buffer.is_empty());
}

#[test]
fn highlighter_tracks_shell_escape_prompt_state() {
    let shell_prompt_hidden = Arc::new(AtomicBool::new(false));
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let highlighter = WolframHighlighter::new(
        builtin_symbol_set(),
        test_user_symbols(),
        Some(source.known_qualified_symbols.clone()),
        Some(source.highlighter_lookup()),
        ThemeHandle::builtin(Theme::dark()),
        shell_prompt_hidden.clone(),
    );

    highlighter.highlight(":!echo ok", 2);
    assert!(shell_prompt_hidden.load(Ordering::Relaxed));

    highlighter.highlight("Plot[x]", 7);
    assert!(!shell_prompt_hidden.load(Ordering::Relaxed));
}

#[test]
fn highlighter_uses_shell_styles_only_for_shell_escape() {
    let fragments = highlight_shell_escape_with_command_lookup(
        ":! echo --help \"hello\" # note",
        test_styles(),
        0,
        |command| command == "echo",
    )
    .buffer;

    assert!(fragments.contains(&(test_styles().prompt_left, "! ".to_string())));
    assert!(!fragments.iter().any(|(_, fragment)| fragment == ":!"));
    assert!(!fragments.iter().any(|(_, fragment)| fragment == ":"));
    assert!(fragments.contains(&(test_styles().completion_command, "echo".to_string())));
    assert!(fragments.contains(&(test_styles().completion_option, "--help".to_string())));
    assert!(fragments.contains(&(test_styles().string, "\"hello\"".to_string())));
    assert!(fragments.contains(&(test_styles().comment, "# note".to_string())));
}

#[test]
fn shell_highlighter_colors_only_known_first_token_as_command() {
    let known = highlight_shell_escape_with_command_lookup(
        ":! known --flag",
        test_styles(),
        0,
        |command| command == "known",
    )
    .buffer;
    assert!(known.contains(&(test_styles().completion_command, "known".to_string())));
    assert!(known.contains(&(test_styles().completion_option, "--flag".to_string())));

    let unknown = highlight_shell_escape_with_command_lookup(
        ":! unknown --flag",
        test_styles(),
        0,
        |command| command == "known",
    )
    .buffer;
    assert!(unknown.contains(&(nu_ansi_term::Style::new(), "unknown".to_string())));
    assert!(!unknown.contains(&(test_styles().completion_command, "unknown".to_string())));
    assert!(unknown.contains(&(test_styles().completion_option, "--flag".to_string())));
}

#[test]
fn highlighter_keeps_display_width_aligned_when_shell_escape_has_real_space() {
    let text = ":! x";
    let highlighted = highlighted_text_at_cursor(text, text.len());

    assert_eq!(highlighted, "!  x");
    assert_eq!(highlighted.chars().count(), text.chars().count());
}

#[test]
fn highlighter_reveals_shell_escape_marker_only_when_cursor_is_left_or_inside_it() {
    for cursor in [0, 1] {
        assert_eq!(
            highlighted_fragments_at_cursor(":!echo", cursor),
            vec![
                (
                    nu_ansi_term::Style::new().fg(nu_ansi_term::Color::Cyan),
                    ":".to_string()
                ),
                (nu_ansi_term::Style::new(), "!echo".to_string())
            ]
        );
    }

    for cursor in [2, 3] {
        assert_eq!(
            highlighted_fragments_at_cursor(":!echo", cursor)[0],
            (test_styles().prompt_left, "! ".to_string())
        );
    }
}

#[test]
fn highlighter_does_not_assume_system_context_symbols_are_defined() {
    let builtin: HashSet<String> = HashSet::new();
    let user: HashSet<String> = HashSet::new();
    let style = style_of("System`Foo[x]", &builtin, &user, "System`Foo");
    assert_eq!(style, test_styles().undefined_symbol);
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
fn dynamic_highlighter_does_not_trust_stale_local_assignments() {
    let user_symbols = Arc::new(Mutex::new(HashSet::from(["zz".to_string()])));
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols.clone(),
    );
    let highlighter = WolframHighlighter::new(
        builtin_symbol_set(),
        user_symbols,
        Some(source.known_qualified_symbols.clone()),
        Some(source.highlighter_lookup()),
        ThemeHandle::builtin(Theme::dark()),
        Arc::new(AtomicBool::new(false)),
    );

    let style = highlighter
        .highlight("zz", 2)
        .buffer
        .into_iter()
        .find_map(|(style, fragment)| (fragment == "zz").then_some(style))
        .expect("zz should be present in highlighted output");
    assert_eq!(style, test_styles().undefined_symbol);
}

#[test]
fn highlighter_colors_symbols_outside_global_and_internal_contexts() {
    let builtin: HashSet<String> = ["Plot".to_string()].into_iter().collect();
    let user: HashSet<String> = ["OtherContext`x".to_string()].into_iter().collect();
    let text = "Global`x + Internal`x + OtherContext`x + OtherContext`unknown";

    assert_eq!(
        style_of(text, &builtin, &user, "Global`x"),
        test_styles().undefined_symbol
    );
    assert_eq!(
        style_of(text, &builtin, &user, "Internal`x"),
        test_styles().undefined_symbol
    );
    assert_eq!(
        style_of(text, &builtin, &user, "OtherContext`x"),
        test_styles().user_symbol
    );
    assert_eq!(
        style_of(text, &builtin, &user, "OtherContext`unknown"),
        test_styles().undefined_symbol
    );
}

#[test]
fn highlighter_colors_exact_custom_context_symbols_from_completion() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "OtherContext`known".to_string(),
            vec![CompletionItem {
                value: "known".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("OtherContext`".to_string()),
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
    let lookup = source.highlighter_lookup();
    lookup.request("OtherContext`known");
    wait_until(|| {
        lookup.request("OtherContext`known");
        source
            .known_qualified_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains("OtherContext`known")
    });

    let builtin = HashSet::new();
    let user = HashSet::new();
    let known = source
        .known_qualified_symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let text = "OtherContext`known + OtherContext`unknown";
    assert_eq!(
        style_of_with_known(text, &builtin, &user, Some(&known), "OtherContext`known"),
        test_styles().user_symbol
    );
    assert_eq!(
        style_of_with_known(text, &builtin, &user, Some(&known), "OtherContext`unknown"),
        test_styles().undefined_symbol
    );
    assert_eq!(
        style_of_with_known("known", &builtin, &user, Some(&known), "known"),
        test_styles().undefined_symbol
    );
}

#[test]
fn highlighter_colors_unqualified_package_symbols_from_completion() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "SQLSelect".to_string(),
            vec![CompletionItem {
                value: "SQLSelect".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("DatabaseLink`".to_string()),
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
    let lookup = source.highlighter_lookup();
    lookup.request("SQLSelect");
    wait_until(|| {
        lookup.request("SQLSelect");
        source
            .known_qualified_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains("SQLSelect")
    });

    let builtin = HashSet::new();
    let user = HashSet::new();
    let known = source
        .known_qualified_symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(
        style_of_with_known("SQLSelect", &builtin, &user, Some(&known), "SQLSelect"),
        test_styles().user_symbol
    );
}

#[test]
fn highlighter_colors_only_exact_one_and_two_letter_names() {
    let symbol = |value: &str| CompletionItem {
        value: value.to_string(),
        kind: CompletionKind::Symbol,
        frequency: None,
        context: Some("Package`".to_string()),
    };
    let backend = FakeBackend {
        symbols: HashMap::from([
            ("x".to_string(), vec![symbol("xDefined")]),
            ("y".to_string(), vec![symbol("y")]),
            ("xx".to_string(), vec![symbol("xxDefined")]),
            ("yy".to_string(), vec![symbol("yy")]),
        ]),
        details: HashMap::new(),
        delay: Duration::ZERO,
        details_delay: Duration::ZERO,
    };
    let source = CompletionSource::with_backend(
        Arc::new(backend),
        Arc::new(AtomicU64::new(0)),
        test_user_symbols(),
    );
    let lookup = source.highlighter_lookup();
    for name in ["x", "y", "xx", "yy"] {
        lookup.prefetch(name, Duration::from_secs(1));
    }

    let builtin = HashSet::new();
    let user = HashSet::new();
    let known = source
        .known_qualified_symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(!known.contains("x"));
    assert!(!known.contains("xx"));
    assert!(known.contains("y"));
    assert!(known.contains("yy"));
    for name in ["x", "xx"] {
        assert_eq!(
            style_of_with_known(name, &builtin, &user, Some(&known), name),
            test_styles().undefined_symbol
        );
    }
    for name in ["y", "yy"] {
        assert_eq!(
            style_of_with_known(name, &builtin, &user, Some(&known), name),
            test_styles().user_symbol
        );
    }
}

#[test]
fn highlighter_checks_each_symbol_independently() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "abKnown".to_string(),
            vec![CompletionItem {
                value: "abKnown".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("Package`".to_string()),
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
    let lookup = source.highlighter_lookup();
    lookup.prefetch("abUnknown", Duration::from_secs(1));
    lookup.prefetch("abKnown", Duration::from_secs(1));

    let known = source
        .known_qualified_symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert!(!known.contains("abUnknown"));
    assert!(known.contains("abKnown"));
}

#[test]
fn highlighter_colors_precise_package_variable_from_completion() {
    let backend = FakeBackend {
        symbols: HashMap::from([(
            "DatabaseLink`$SQLTimeout".to_string(),
            vec![CompletionItem {
                value: "$SQLTimeout".to_string(),
                kind: CompletionKind::Symbol,
                frequency: None,
                context: Some("DatabaseLink`".to_string()),
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
    let lookup = source.highlighter_lookup();
    lookup.request("DatabaseLink`$SQLTimeout");
    wait_until(|| {
        lookup.request("DatabaseLink`$SQLTimeout");
        source
            .known_qualified_symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains("DatabaseLink`$SQLTimeout")
    });

    let builtin = HashSet::new();
    let user = HashSet::new();
    let known = source
        .known_qualified_symbols
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let text = "DatabaseLink`$SQLTimeout + DatabaseLink`$Other";
    assert_eq!(
        style_of_with_known(
            text,
            &builtin,
            &user,
            Some(&known),
            "DatabaseLink`$SQLTimeout"
        ),
        test_styles().user_symbol
    );
    assert_eq!(
        style_of_with_known(text, &builtin, &user, Some(&known), "DatabaseLink`$Other"),
        test_styles().undefined_symbol
    );
}

/// Not a correctness test: measures the per-keystroke hot path (menu +
/// hinter completion passes plus a highlight) so optimizations can be
/// compared. Run with:
/// `cargo test --release typing_hot_path_benchmark -- --ignored --nocapture`
#[test]
#[ignore]
fn typing_hot_path_benchmark() {
    let user_symbols = test_user_symbols();
    {
        let mut symbols = user_symbols
            .lock()
            .expect("user symbols lock should be available");
        for index in 0..50 {
            symbols.insert(format!("myVariable{index}"));
        }
    }
    let source = CompletionSource::with_backend(
        Arc::new(FakeBackend::empty()),
        Arc::new(AtomicU64::new(0)),
        user_symbols.clone(),
    );
    {
        let mut known = source
            .known_qualified_symbols
            .lock()
            .expect("known symbols lock should be available");
        for index in 0..2000 {
            known.insert(format!("SomePackage`Subcontext`Symbol{index}"));
        }
    }
    let theme = ThemeHandle::builtin(Theme::dark());
    let mut menu_completer = WolframCompleter::new(source.clone(), theme.clone());
    let mut hinter_completer = WolframCompleter::new(source.clone(), theme.clone());
    let highlighter = WolframHighlighter::new(
        builtin_symbol_set(),
        user_symbols,
        Some(source.known_qualified_symbols.clone()),
        Some(source.highlighter_lookup()),
        theme,
        Arc::new(AtomicBool::new(false)),
    );

    let line = "myFunc[alpha_] := ListLinePlot[Table[alpha x, {x, 0, 10}]] + custom`thing";
    let rounds = 200;

    let started = Instant::now();
    for _ in 0..rounds {
        for pos in 1..=line.len() {
            let _ = menu_completer.complete(&line[..pos], pos);
            let _ = hinter_completer.complete(&line[..pos], pos);
            let _ = highlighter.highlight(&line[..pos], pos);
        }
    }
    let elapsed = started.elapsed();
    let keystrokes = rounds * line.len();
    println!(
        "typing hot path: {keystrokes} keystrokes in {elapsed:?} ({:.3} ms/keystroke)",
        elapsed.as_secs_f64() * 1000.0 / keystrokes as f64
    );
}
