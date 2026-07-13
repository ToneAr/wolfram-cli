use std::{
    collections::HashSet,
    io::{self, IsTerminal},
    process::ExitStatus,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use reedline::{
    FileBackedHistory, Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineMenu, Signal,
};

use crate::{
    commands::{
        CommandAction, ConfigMode, execute_repl_command, execute_shell_escape, run_shell_escape,
        top_level_run_command,
    },
    completion::{
        CompletionSource, GhostCompletionSelection, WolframCompleter, WolframCompletionHinter,
        builtin_symbol_set,
    },
    editor::{
        HistoryTrigger, WolframPrompt, WolframValidator, completion_edit_mode, completion_menu,
        history_menu, history_path, history_primed_edit_mode,
    },
    highlighter::WolframHighlighter,
    kernel::{
        KernelClient, KernelConnection, SharedKernel, WolframVersions, kernel_input_prompt,
        kernel_may_be_slow_to_respond, kernel_status, lock_kernel, spawn_kernel_warmup,
        wolfram_versions,
    },
    native_wstp::KernelInputRequest,
    theme::{ThemeHandle, ThemeRegistry, UserConfig, selected_theme},
    wolfram_syntax::{loaded_context_names, remember_user_symbols},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReplFeatures {
    pub(crate) kernel_warmup: bool,
    pub(crate) dynamic_completion: bool,
    pub(crate) completion_ghost_text: bool,
    pub(crate) completion_menu: bool,
    pub(crate) semantic_highlighting: bool,
    pub(crate) history: bool,
}

pub(crate) fn run_repl(
    use_color: bool,
    show_welcome: bool,
    show_prompts: bool,
    connection: KernelConnection,
    config: UserConfig,
    config_mode: ConfigMode,
    features: ReplFeatures,
) -> Result<()> {
    let completion_epoch = Arc::new(AtomicU64::new(0));
    let shell_prompt_hidden = Arc::new(AtomicBool::new(false));
    let user_symbols = Arc::new(Mutex::new(HashSet::new()));
    let kernel = Arc::new(Mutex::new(KernelClient::with_connection(connection)?));
    if features.kernel_warmup {
        spawn_kernel_warmup(kernel.clone());
    }
    let theme_registry = match config_mode {
        ConfigMode::User => ThemeRegistry::load(),
        ConfigMode::Ephemeral => ThemeRegistry::builtin_only(),
    };
    let initial_theme = selected_theme(use_color, &theme_registry, config.theme.as_deref());
    let theme = match config_mode {
        ConfigMode::User => ThemeHandle::new(initial_theme, theme_registry),
        ConfigMode::Ephemeral => ThemeHandle::ephemeral(initial_theme, theme_registry),
    };
    if show_welcome {
        let versions = wolfram_versions();
        print_welcome(use_color, &versions, &theme);
    }
    let completion_source = features.dynamic_completion.then(|| {
        CompletionSource::new(
            kernel.clone(),
            completion_epoch.clone(),
            user_symbols.clone(),
        )
    });
    let symbol_lookup = completion_source
        .as_ref()
        .map(CompletionSource::highlighter_lookup);
    let known_qualified_symbols = completion_source
        .as_ref()
        .map(|source| source.known_qualified_symbols.clone());
    let symbol_set = builtin_symbol_set();
    let history_trigger = HistoryTrigger::new();
    let completion_menu_enabled = features.dynamic_completion && features.completion_menu;
    let completion_ghost_text_enabled =
        features.dynamic_completion && features.completion_ghost_text;
    let ghost_completion_selection = (completion_ghost_text_enabled && !completion_menu_enabled)
        .then(GhostCompletionSelection::new);
    let mut line_editor = Reedline::create()
        .use_kitty_keyboard_enhancement(true)
        .with_ansi_colors(use_color)
        .with_visual_selection_style(theme.current().styles().visual_selection)
        .with_highlighter(Box::new(WolframHighlighter::new(
            symbol_set,
            user_symbols.clone(),
            known_qualified_symbols,
            symbol_lookup.clone(),
            theme.clone(),
            shell_prompt_hidden.clone(),
            features.semantic_highlighting,
        )))
        .with_validator(Box::new(WolframValidator));
    if features.history {
        line_editor = line_editor
            .with_history(Box::new(FileBackedHistory::with_file(
                2000,
                history_path()?,
            )?))
            .with_menu(ReedlineMenu::HistoryMenu(Box::new(history_menu(
                theme.clone(),
            ))))
            .with_edit_mode(history_primed_edit_mode(
                completion_edit_mode(completion_menu_enabled),
                history_trigger.clone(),
                ghost_completion_selection.clone(),
            ));
    } else {
        line_editor = line_editor
            .with_history(Box::new(FileBackedHistory::new(0)?))
            .with_edit_mode(Box::new(completion_edit_mode(completion_menu_enabled)));
    }
    if let Some(completion_source) = completion_source.as_ref() {
        line_editor = line_editor.with_completer(Box::new(WolframCompleter::new(
            completion_source.clone(),
            theme.clone(),
        )));
        if completion_ghost_text_enabled {
            line_editor = line_editor.with_hinter(Box::new(WolframCompletionHinter::new(
                completion_source.clone(),
                theme.clone(),
                ghost_completion_selection.unwrap_or_else(GhostCompletionSelection::new),
            )));
        }
        if completion_menu_enabled {
            line_editor = line_editor.with_menu(ReedlineMenu::EngineCompleter(Box::new(
                completion_menu(theme.clone()),
            )));
        }
    }
    loop {
        let prompt = WolframPrompt {
            input_prompt: kernel_input_prompt(&kernel)?.unwrap_or_else(|| "In[1]:= ".to_string()),
            kernel_status: kernel_status(&kernel)?,
            theme: theme.clone(),
            show_prompt: show_prompts,
            shell_prompt_hidden: shell_prompt_hidden.clone(),
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
                if let Some(command) = input.strip_prefix(":!") {
                    execute_shell_escape(command);
                    continue;
                }
                if input.starts_with(':') {
                    match execute_repl_command(input, &theme, use_color, config_mode) {
                        CommandAction::Quit => break,
                        CommandAction::OpenHistory => {
                            if features.history {
                                history_trigger.arm();
                                println!("(press any key to open the history browser)");
                            } else {
                                println!("History is disabled in lightweight mode.");
                            }
                        }
                        CommandAction::Continue => {}
                    }
                    continue;
                }
                let evaluation_input = if let Some(command) = top_level_run_command(input) {
                    let status = run_shell_escape(&command)?;
                    shell_exit_code(status).to_string()
                } else {
                    input.to_string()
                };
                let (mut kernel, may_be_slow) = lock_kernel_for_repl_input(&kernel)?;
                if may_be_slow {
                    println!("\n{}: Kernel is starting up", "Wolfie::init");
                }
                let mut kernel_input_handler = |request: &KernelInputRequest| {
                    read_kernel_input(&mut line_editor, request, theme.clone(), show_prompts)
                };
                kernel.evaluate_repl_input(
                    &evaluation_input,
                    &theme,
                    &mut kernel_input_handler,
                    show_prompts,
                )?;
                drop(kernel);
                remember_user_symbols(input, &user_symbols);
                if features.dynamic_completion {
                    completion_epoch.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(symbol_lookup) = symbol_lookup.as_ref() {
                    for context in loaded_context_names(input) {
                        symbol_lookup.prefetch(&context, Duration::from_millis(50));
                    }
                }
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
        }
    }

    Ok(())
}

const KERNEL_INIT_WARNING_GRACE: Duration = Duration::from_millis(150);

fn shell_exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn lock_kernel_for_repl_input(
    kernel: &SharedKernel,
) -> Result<(std::sync::MutexGuard<'_, KernelClient>, bool)> {
    let started_waiting = Instant::now();
    loop {
        match kernel.try_lock() {
            Ok(kernel) => {
                let may_be_slow = kernel_may_be_slow_to_respond(kernel.status());
                return Ok((kernel, may_be_slow));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if started_waiting.elapsed() >= KERNEL_INIT_WARNING_GRACE {
                    return Ok((lock_kernel(kernel)?, true));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(anyhow::anyhow!("kernel session lock was poisoned"));
            }
        }
    }
}

const WELCOME_BANNER: &str = r#"
╭───────────────────────────────────────────────────────╮
│                              ,,      ,...,,           │
│ `7MMF'     A     `7MF'     `7MM    .d' ""db           │
│   `MA     ,MA     ,V         MM    dM`                │
│    VM:   ,VVM:   ,V ,pW"Wq.  MM   mMMmm`7MM  .gP"Ya   │
│     MM.  M' MM.  M'6W'   `Wb MM    MM    MM ,M'   Yb  │
│     `MM A'  `MM A' 8M     M8 MM    MM    MM 8M""""""  │
│      :MM;    :MM;  YA.   ,A9 MM    MM    MM YM.    ,  │
│       VF      VF    `Ybmd9'.JMML..JMML..JMML.`Mbmmd'  │
│                                                       │
│          Wolfram Friendly Interactive Shell           │
╰───────────────────────────────────────────────────────╯
"#;

const WELCOME_BANNER_WIDTH: u16 = 64;

const WELCOME_GRADIENT: [nu_ansi_term::Rgb; 6] = [
    nu_ansi_term::Rgb::new(255, 82, 119),
    nu_ansi_term::Rgb::new(230, 90, 100),
    nu_ansi_term::Rgb::new(180, 90, 80),
    nu_ansi_term::Rgb::new(140, 90, 80),
    nu_ansi_term::Rgb::new(170, 130, 100),
    nu_ansi_term::Rgb::new(130, 120, 110),
];

fn print_welcome(use_color: bool, versions: &WolframVersions, theme: &ThemeHandle) {
    if use_color {
        if terminal_can_fit_welcome_banner() {
            print_gradient_welcome(versions, theme);
        } else {
            println!("\n    Wolfram Friendly Interactive Shell\n");
            print_styled_welcome_details(versions, theme);
        }
    } else {
        print_plain_welcome(versions);
    }
}

fn terminal_can_fit_welcome_banner() -> bool {
    if !io::stdout().is_terminal() {
        return true;
    }

    crossterm::terminal::size()
        .map(|(columns, _)| columns >= WELCOME_BANNER_WIDTH)
        .unwrap_or(true)
}

fn print_plain_welcome(versions: &WolframVersions) {
    println!("Wolfie");
    println!("TUI Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Wolfram Kernel: {}", versions.kernel);
    println!("WolframScript: {}", versions.wolframscript);
    println!("Type :help for commands, :quit or Ctrl-D to quit.\n");
}

fn print_gradient_welcome(versions: &WolframVersions, theme: &ThemeHandle) {
    // let animate = io::stdout().is_terminal();
    let lines: Vec<_> = WELCOME_BANNER.trim_matches('\n').lines().collect();

    println!();
    for (line_index, line) in lines.iter().enumerate() {
        print_gradient_line(line, line_index, lines.len());
        println!();

        // if animate {
        //     let _ = io::stdout().flush();
        //     thread::sleep(Duration::from_millis(18));
        // }
    }

    print_styled_welcome_details(versions, theme);
}

fn print_styled_welcome_details(versions: &WolframVersions, theme: &ThemeHandle) {
    let styles = theme.current().styles();
    let accent = styles.prompt_left;
    let title = nu_ansi_term::Style::new()
        .bold()
        .fg(nu_ansi_term::Color::Rgb(255, 255, 255));
    let muted = nu_ansi_term::Style::new().fg(nu_ansi_term::Color::DarkGray);

    println!();
    println!(
        "  {} {} {}",
        accent.paint("◆"),
        title.paint("Wolfie        "),
        muted.paint(format!("{}", env!("CARGO_PKG_VERSION")))
    );
    println!(
        "  {} {} {}",
        accent.paint("◆"),
        title.paint("WolframKernel "),
        muted.paint(&versions.kernel)
    );
    println!(
        "  {} {} {}",
        accent.paint("◆"),
        title.paint("WolframScript "),
        muted.paint(&versions.wolframscript)
    );
    println!();
    println!(
        "  {}",
        muted.paint("Type :help for commands, :quit or Ctrl-D to quit.")
    );
    println!();
}

fn print_gradient_line(line: &str, line_index: usize, line_count: usize) {
    let line_width = line.chars().count().max(1);
    let gradient_width = line_width + line_count * 3;

    for (char_index, ch) in line.chars().enumerate() {
        if ch.is_whitespace() {
            print!("{ch}");
            continue;
        }

        let color = gradient_color(char_index + line_index * 10, gradient_width);
        print!(
            "{}",
            nu_ansi_term::Style::new()
                .bold()
                .fg(color)
                .paint(ch.to_string())
        );
    }
}

fn gradient_color(position: usize, width: usize) -> nu_ansi_term::Color {
    let span = width.saturating_sub(1).max(1) as f32;
    let scaled = position as f32 / span * (WELCOME_GRADIENT.len() - 1) as f32;
    let lower_index = (scaled.floor() as usize).min(WELCOME_GRADIENT.len() - 2);
    let upper_index = lower_index + 1;
    let mix = scaled - lower_index as f32;

    let rgb =
        nu_ansi_term::Gradient::new(WELCOME_GRADIENT[lower_index], WELCOME_GRADIENT[upper_index])
            .at(mix);

    nu_ansi_term::Color::Rgb(rgb.r, rgb.g, rgb.b)
}

fn read_kernel_input(
    line_editor: &mut Reedline,
    request: &KernelInputRequest,
    theme: ThemeHandle,
    show_prompt: bool,
) -> Result<Option<String>> {
    let prompt = KernelInputPrompt {
        text: request.prompt.clone(),
        theme,
        show_prompt,
    };

    match line_editor.read_line(&prompt)? {
        Signal::Success(input) => Ok(Some(input)),
        Signal::CtrlC => Ok(Some(String::new())),
        Signal::CtrlD => Ok(None),
    }
}

struct KernelInputPrompt {
    text: String,
    theme: ThemeHandle,
    show_prompt: bool,
}

impl Prompt for KernelInputPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
        if !self.show_prompt {
            return "".into();
        }

        self.theme
            .current()
            .styles()
            .prompt_left
            .paint(&self.text)
            .to_string()
            .into()
    }

    fn render_prompt_right(&self) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> std::borrow::Cow<'_, str> {
        "".into()
    }

    fn render_prompt_multiline_indicator(&self) -> std::borrow::Cow<'_, str> {
        if !self.show_prompt {
            return "".into();
        }

        self.theme
            .current()
            .styles()
            .prompt_multiline_text
            .paint(" ".repeat(self.text.chars().count()))
            .to_string()
            .into()
    }

    fn render_prompt_history_search_indicator(
        &self,
        _history_search: PromptHistorySearch,
    ) -> std::borrow::Cow<'_, str> {
        "".into()
    }
}
