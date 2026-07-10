use std::{
    collections::HashSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use reedline::{
    FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineMenu, Signal,
};

use crate::{
    commands::{CommandAction, execute_repl_command},
    completion::{CompletionSource, WolframCompleter, builtin_symbol_names},
    editor::{
        HistoryTrigger, WolframPrompt, WolframValidator, completion_edit_mode, completion_menu,
        history_menu, history_path, history_primed_edit_mode,
    },
    frontend::{FrontEndClient, frontend_status},
    highlighter::WolframHighlighter,
    kernel::{
        KernelClient, kernel_input_prompt, kernel_may_be_slow_to_respond, kernel_status,
        lock_kernel, spawn_kernel_warmup,
    },
    native_wstp::KernelInputRequest,
    theme::{Theme, ThemeHandle},
    wolfram_syntax::remember_user_symbols,
};

pub(crate) fn run_repl(enable_frontend: bool, use_color: bool) -> Result<()> {
    let history = history_path()?;
    let completion_epoch = Arc::new(AtomicU64::new(0));
    let user_symbols = Arc::new(Mutex::new(HashSet::new()));
    let kernel = Arc::new(Mutex::new(KernelClient::new()?));
    let frontend = if enable_frontend {
        Some(Arc::new(Mutex::new(FrontEndClient::new())))
    } else {
        None
    };
    spawn_kernel_warmup(kernel.clone());
    print_welcome(use_color);
    let initial_theme = if use_color { Theme::Dark } else { Theme::Plain };
    let theme = ThemeHandle::new(initial_theme);
    let completion_source = CompletionSource::new(
        kernel.clone(),
        completion_epoch.clone(),
        user_symbols.clone(),
    );
    let symbol_set = builtin_symbol_names().collect();
    let history_trigger = HistoryTrigger::new();
    let mut line_editor = Reedline::create()
        .use_kitty_keyboard_enhancement(true)
        .with_ansi_colors(use_color)
        .with_history(Box::new(FileBackedHistory::with_file(2000, history)?))
        .with_highlighter(Box::new(WolframHighlighter::new(
            symbol_set,
            user_symbols.clone(),
            theme.clone(),
        )))
        .with_completer(Box::new(WolframCompleter::new(
            completion_source,
            theme.clone(),
        )))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu())))
        .with_menu(ReedlineMenu::HistoryMenu(Box::new(history_menu())))
        .with_validator(Box::new(WolframValidator))
        .with_edit_mode(history_primed_edit_mode(
            completion_edit_mode(),
            history_trigger.clone(),
        ));
    loop {
        let prompt = WolframPrompt {
            input_prompt: kernel_input_prompt(&kernel)?.unwrap_or_else(|| "In[1]:= ".to_string()),
            kernel_status: kernel_status(&kernel)?,
            frontend_status: frontend_status(frontend.as_ref())?,
        };
        match line_editor.read_line(&prompt)? {
            Signal::Success(input) => {
                let input = input.trim();
                if input.is_empty() {
                    continue;
                }
                if matches!(input, "Exit" | "Quit") {
                    break;
                }
                if input.starts_with(':') {
                    match execute_repl_command(input, &theme, use_color) {
                        CommandAction::Quit => break,
                        CommandAction::OpenHistory => {
                            history_trigger.arm();
                            println!("(press any key to open the history browser)");
                        }
                        CommandAction::Continue => {}
                    }
                    continue;
                }
                if kernel_may_be_slow_to_respond(kernel_status(&kernel)?) {
                    println!(
                        "(kernel is still starting up; the first response can take a few seconds)"
                    );
                }
                let mut kernel_input_handler =
                    |request: &KernelInputRequest| read_kernel_input(&mut line_editor, request);
                lock_kernel(&kernel)?.evaluate_repl_input(
                    input,
                    &theme,
                    &mut kernel_input_handler,
                )?;
                remember_user_symbols(input, &user_symbols);
                completion_epoch.fetch_add(1, Ordering::Relaxed);
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
        }
    }

    Ok(())
}

fn print_welcome(use_color: bool) {
    if use_color {
        println!(
            "{}{}",
            nu_ansi_term::Style::new()
                .bold()
                .underline()
                .fg(nu_ansi_term::Color::Red)
                .paint("Wolfram"),
            nu_ansi_term::Style::new()
                .bold()
                .underline()
                .fg(nu_ansi_term::Color::DarkGray)
                .paint("Shell")
        );
    } else {
        println!("WolframShell");
    }
    println!("TUI Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Type :help for commands, :quit or Ctrl-D to quit.\n");
}

fn read_kernel_input(
    line_editor: &mut Reedline,
    request: &KernelInputRequest,
) -> Result<Option<String>> {
    let prompt = KernelInputPrompt {
        text: request.prompt.clone(),
    };

    match line_editor.read_line(&prompt)? {
        Signal::Success(input) => Ok(Some(input)),
        Signal::CtrlC => Ok(Some(String::new())),
        Signal::CtrlD => Ok(None),
    }
}

struct KernelInputPrompt {
    text: String,
}

impl Prompt for KernelInputPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        self.text.as_str().into()
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        " ".repeat(self.text.chars().count()).into()
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        "".into()
    }
}
