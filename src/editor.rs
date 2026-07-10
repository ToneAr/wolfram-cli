use std::{
    cell::Cell,
    env,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyEvent};
use reedline::{
    Completer, EditCommand, EditMode, Editor, Emacs, IdeMenu, KeyCode, KeyModifiers, ListMenu,
    Menu, MenuBuilder, MenuEvent, Painter, Prompt, PromptEditMode, PromptHistorySearch,
    ReedlineEvent, ReedlineRawEvent, Suggestion, ValidationResult, Validator,
    default_emacs_keybindings,
};

use crate::{
    frontend::FrontEndStatus,
    kernel::KernelStatus,
    wolfram_syntax::{completion_is_disabled_at_cursor, wolfram_input_is_incomplete},
};

const COMPLETION_MENU: &str = "completion_menu";
pub(crate) const HISTORY_MENU: &str = "history_menu";

pub(crate) struct WolframPrompt {
    pub(crate) input_prompt: String,
    pub(crate) kernel_status: KernelStatus,
    pub(crate) frontend_status: FrontEndStatus,
}

impl Prompt for WolframPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        self.input_prompt.as_str().into()
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        format!(
            "Kernel: {} | FE: {}",
            self.kernel_status, self.frontend_status
        )
        .into()
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        let width = self.input_prompt.chars().count();
        format!("{}> ", " ".repeat(width.saturating_sub(2))).into()
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        "".into()
    }
}

pub(crate) fn completion_menu() -> StringAwareIdeMenu {
    StringAwareIdeMenu::new(
        IdeMenu::default()
            .with_name(COMPLETION_MENU)
            .with_only_buffer_difference(false)
            .with_max_completion_height(6)
            .with_max_description_height(6),
    )
}

pub(crate) fn history_menu() -> ListMenu {
    ListMenu::default()
        .with_name(HISTORY_MENU)
        .with_page_size(10)
        .with_max_entry_lines(1)
}

pub(crate) struct StringAwareIdeMenu {
    inner: IdeMenu,
}

impl StringAwareIdeMenu {
    fn new(inner: IdeMenu) -> Self {
        Self { inner }
    }

    fn deactivate_if_completion_disabled(
        &mut self,
        editor: &mut Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        if !completion_is_disabled_at_cursor(
            editor.get_buffer(),
            editor.line_buffer().insertion_point(),
        ) {
            return false;
        }

        self.inner.update_values(editor, completer);
        self.inner.menu_event(MenuEvent::Deactivate);
        true
    }
}

impl Menu for StringAwareIdeMenu {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn indicator(&self) -> &str {
        self.inner.indicator()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn menu_event(&mut self, event: MenuEvent) {
        self.inner.menu_event(event);
    }

    fn can_quick_complete(&self) -> bool {
        self.inner.can_quick_complete()
    }

    fn can_partially_complete(
        &mut self,
        values_updated: bool,
        editor: &mut Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        if self.deactivate_if_completion_disabled(editor, completer) {
            return false;
        }

        self.inner
            .can_partially_complete(values_updated, editor, completer)
    }

    fn update_values(&mut self, editor: &mut Editor, completer: &mut dyn Completer) {
        if !self.deactivate_if_completion_disabled(editor, completer) {
            self.inner.update_values(editor, completer);
        }
    }

    fn update_working_details(
        &mut self,
        editor: &mut Editor,
        completer: &mut dyn Completer,
        painter: &Painter,
    ) {
        if !self.deactivate_if_completion_disabled(editor, completer) {
            self.inner
                .update_working_details(editor, completer, painter);
        }
    }

    fn replace_in_buffer(&self, editor: &mut Editor) {
        self.inner.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.inner.menu_required_lines(terminal_columns)
    }

    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        self.inner.menu_string(available_lines, use_ansi_coloring)
    }

    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }

    fn set_cursor_pos(&mut self, pos: (u16, u16)) {
        self.inner.set_cursor_pos(pos);
    }
}

pub(crate) fn completion_edit_mode() -> Emacs {
    let mut keybindings = default_emacs_keybindings();

    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        accept_or_open_completion(),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('i'),
        accept_or_open_completion(),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('\t'),
        accept_or_open_completion(),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char(' '),
        ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char('r'),
        ReedlineEvent::Menu(HISTORY_MENU.to_string()),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Enter,
        ReedlineEvent::Multiple(vec![ReedlineEvent::Esc, ReedlineEvent::Enter]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Enter,
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        ]),
    );
    keybindings.add_binding(KeyModifiers::NONE, KeyCode::BackTab, insert_tab_character());

    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Left,
        ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select: false }]),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Right,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::HistoryHintComplete,
            ReedlineEvent::Edit(vec![EditCommand::MoveRight { select: false }]),
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        insert_tab_character(),
    );

    for byte in b'a'..=b'z' {
        let lower = byte as char;
        let upper = lower.to_ascii_uppercase();

        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(lower),
            insert_and_open_completion(lower),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::Char(lower),
            insert_and_open_completion(upper),
        );
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(upper),
            insert_and_open_completion(upper),
        );
    }

    for ch in ['$', '`'] {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(ch),
            insert_and_open_completion(ch),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::Char(ch),
            insert_and_open_completion(ch),
        );
    }

    for ch in [
        ' ', '[', ']', '{', '}', '(', ')', ',', ';', '+', '-', '*', '/', '^', '=', '<', '>', '!',
        '&', '|', '@', '#', '%', '\'', '"', ':', '.', '?', '_', '~', '\\',
    ] {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(ch),
            insert_and_close_completion(ch),
        );
        keybindings.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::Char(ch),
            insert_and_close_completion(ch),
        );
    }

    Emacs::new(keybindings)
}

fn accept_or_open_completion() -> ReedlineEvent {
    ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
        ReedlineEvent::Enter,
    ])
}

fn insert_and_open_completion(ch: char) -> ReedlineEvent {
    ReedlineEvent::Multiple(vec![
        ReedlineEvent::Edit(vec![EditCommand::InsertChar(ch)]),
        ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
    ])
}

fn insert_tab_character() -> ReedlineEvent {
    ReedlineEvent::Edit(vec![EditCommand::InsertChar('\t')])
}

fn insert_and_close_completion(ch: char) -> ReedlineEvent {
    ReedlineEvent::Multiple(vec![
        ReedlineEvent::Edit(vec![EditCommand::InsertChar(ch)]),
        ReedlineEvent::Esc,
    ])
}

/// Handle used to arm the history menu so it opens on the next keystroke.
///
/// Reedline only learns what was typed (e.g. `:history`) after `Enter` ends
/// that `read_line` call, and it has no API to pop a menu open before any
/// further input arrives on the *next* prompt. Arming this trigger makes the
/// very next key event open the history menu; that keystroke is consumed to
/// open it rather than also being applied, since bundling activation with an
/// edit in the same tick corrupts `ListMenu`'s buffer-diff filter baseline.
#[derive(Clone)]
pub(crate) struct HistoryTrigger(Arc<AtomicBool>);

impl HistoryTrigger {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn arm(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }
}

struct HistoryPrimedEditMode {
    inner: Emacs,
    trigger: HistoryTrigger,
    /// True while the history menu is open. While active, plain character
    /// keys bypass `inner`'s Wolfram-completion bindings (which insert a
    /// char and then close whatever menu is open, e.g. on operators like
    /// `+`) and instead just insert normally, so typing filters the history
    /// list instead of dismissing it.
    history_active: Cell<bool>,
}

impl EditMode for HistoryPrimedEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw = event.into();

        if self.trigger.take() {
            self.history_active.set(true);
            return ReedlineEvent::Menu(HISTORY_MENU.to_string());
        }
        if is_history_open_key(&raw) {
            self.history_active.set(true);
        }

        if self.history_active.get() {
            // Bypass `inner`'s Wolfram-completion bindings entirely while the
            // history menu is open: those bindings hard-code `Enter` to
            // `Multiple([Esc, Enter])` (so completion suggestions never
            // hijack submission) and close-on-punctuation for symbol
            // completion, neither of which apply to browsing history.
            // Reedline's own engine special-cases plain `Enter`/`Esc` when a
            // menu is active (accept-selection-without-submitting, and
            // cancel, respectively), so returning them undecorated here
            // restores normal menu navigation.
            if is_history_accept_key(&raw) {
                self.history_active.set(false);
                return ReedlineEvent::Enter;
            }
            if is_history_cancel_key(&raw) {
                self.history_active.set(false);
                return ReedlineEvent::Esc;
            }
            if let Some(plain_insert) = plain_char_insert(&raw) {
                return plain_insert;
            }
        }

        let rebuilt = ReedlineRawEvent::convert_from(raw).expect("re-wrapping a non-release event");
        self.inner.parse_event(rebuilt)
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

fn is_history_open_key(raw: &Event) -> bool {
    matches!(
        raw,
        Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn is_history_accept_key(raw: &Event) -> bool {
    matches!(
        raw,
        Event::Key(KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            ..
        })
    )
}

fn is_history_cancel_key(raw: &Event) -> bool {
    matches!(
    	raw,
        Event::Key(KeyEvent {
            code: KeyCode::Esc, ..
        }) |
        Event::Key(KeyEvent {
            code: KeyCode::Char('c') | KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn plain_char_insert(raw: &Event) -> Option<ReedlineEvent> {
    match raw {
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::NONE,
            ..
        }) => Some(ReedlineEvent::Edit(vec![EditCommand::InsertChar(*c)])),
        Event::Key(KeyEvent {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::SHIFT,
            ..
        }) => Some(ReedlineEvent::Edit(vec![EditCommand::InsertChar(
            c.to_ascii_uppercase(),
        )])),
        _ => None,
    }
}

pub(crate) fn history_primed_edit_mode(inner: Emacs, trigger: HistoryTrigger) -> Box<dyn EditMode> {
    Box::new(HistoryPrimedEditMode {
        inner,
        trigger,
        history_active: Cell::new(false),
    })
}

pub(crate) struct WolframValidator;

impl Validator for WolframValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if line.trim_start().starts_with(':') {
            return ValidationResult::Complete;
        }

        if wolfram_input_is_incomplete(line) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Complete
        }
    }
}

pub(crate) fn history_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("could not determine a history directory")?;
    let dir = base.join("wolfsh");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("history"))
}
