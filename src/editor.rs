use std::{env, path::PathBuf};

use anyhow::{Context, Result};
use reedline::{
    Completer, EditCommand, Editor, Emacs, IdeMenu, KeyCode, KeyModifiers, Menu, MenuBuilder,
    MenuEvent, Painter, Prompt, PromptEditMode, PromptHistorySearch, ReedlineEvent, Suggestion,
    ValidationResult, Validator, default_emacs_keybindings,
};

use crate::{
    frontend::FrontEndStatus,
    kernel::KernelStatus,
    wolfram_syntax::{completion_is_disabled_at_cursor, wolfram_input_is_incomplete},
};

const COMPLETION_MENU: &str = "completion_menu";

pub(crate) struct WolframPrompt {
    pub(crate) line_number: usize,
    pub(crate) kernel_status: KernelStatus,
    pub(crate) frontend_status: FrontEndStatus,
}

impl Prompt for WolframPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        format!("In[{}]:= ", self.line_number).into()
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
        "        ".into()
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
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Enter,
            ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
        ]),
    );
    keybindings.add_binding(
        KeyModifiers::CONTROL,
        KeyCode::Char(' '),
        ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
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
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );

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
        ReedlineEvent::MenuPrevious,
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

fn insert_and_open_completion(ch: char) -> ReedlineEvent {
    ReedlineEvent::Multiple(vec![
        ReedlineEvent::Edit(vec![EditCommand::InsertChar(ch)]),
        ReedlineEvent::Menu(COMPLETION_MENU.to_string()),
    ])
}

fn insert_and_close_completion(ch: char) -> ReedlineEvent {
    ReedlineEvent::Multiple(vec![
        ReedlineEvent::Edit(vec![EditCommand::InsertChar(ch)]),
        ReedlineEvent::Esc,
    ])
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
    let dir = base.join("wolfram-cli");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("history"))
}
