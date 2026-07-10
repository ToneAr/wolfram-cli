use std::{
    collections::HashSet,
    io::{self, IsTerminal},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
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
        KernelClient, KernelConnection, SharedKernel, WolframVersions, kernel_input_prompt,
        kernel_may_be_slow_to_respond, kernel_status, lock_kernel, spawn_kernel_warmup,
        wolfram_versions,
    },
    native_wstp::KernelInputRequest,
    theme::{ThemeHandle, ThemeRegistry, selected_theme},
    wolfram_syntax::remember_user_symbols,
};

pub(crate) fn run_repl(
    enable_frontend: bool,
    use_color: bool,
    connection: KernelConnection,
) -> Result<()> {
    let history = history_path()?;
    let completion_epoch = Arc::new(AtomicU64::new(0));
    let user_symbols = Arc::new(Mutex::new(HashSet::new()));
    let kernel = Arc::new(Mutex::new(KernelClient::with_connection(connection)?));
    let frontend = if enable_frontend {
        Some(Arc::new(Mutex::new(FrontEndClient::new())))
    } else {
        None
    };
    let versions = wolfram_versions();
    spawn_kernel_warmup(kernel.clone());
    print_welcome(use_color, &versions);
    let theme_registry = ThemeRegistry::load();
    let initial_theme = selected_theme(use_color, &theme_registry);
    let theme = ThemeHandle::new(initial_theme, theme_registry);
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
        .with_visual_selection_style(theme.current().styles().visual_selection)
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
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu(
            theme.clone(),
        ))))
        .with_menu(ReedlineMenu::HistoryMenu(Box::new(history_menu(
            theme.clone(),
        ))))
        .with_validator(Box::new(WolframValidator))
        .with_edit_mode(history_primed_edit_mode(
            completion_edit_mode(),
            history_trigger.clone(),
        ));
    loop {
        let prompt = WolframPrompt {
            input_prompt: kernel_input_prompt(&kernel)?.unwrap_or_else(|| "In[1]:= ".to_string()),
            kernel_status: kernel_status(&kernel)?,
            _frontend_status: frontend_status(frontend.as_ref())?,
            theme: theme.clone(),
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
                let (mut kernel, may_be_slow) = lock_kernel_for_repl_input(&kernel)?;
                if may_be_slow {
                    println!(
                        "\n{}: Kernel is starting up",
                        nu_ansi_term::Color::Yellow.paint("Wolfie::init")
                    );
                }
                let mut kernel_input_handler = |request: &KernelInputRequest| {
                    read_kernel_input(&mut line_editor, request, theme.clone())
                };
                kernel.evaluate_repl_input(input, &theme, &mut kernel_input_handler)?;
                remember_user_symbols(input, &user_symbols);
                completion_epoch.fetch_add(1, Ordering::Relaxed);
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
        }
    }

    Ok(())
}

const KERNEL_INIT_WARNING_GRACE: Duration = Duration::from_millis(150);

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

fn print_welcome(use_color: bool, versions: &WolframVersions) {
    if use_color {
        if terminal_can_fit_welcome_banner() {
            print_gradient_welcome(versions);
        } else {
            println!("\n    Wolfram Friendly Interactive Shell\n");
            print_styled_welcome_details(versions);
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
    println!("WolframShell");
    println!("TUI Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Wolfram Kernel: {}", versions.kernel);
    println!("WolframScript: {}", versions.wolframscript);
    println!("Type :help for commands, :quit or Ctrl-D to quit.\n");
}

fn print_gradient_welcome(versions: &WolframVersions) {
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

    print_styled_welcome_details(versions);
}

fn print_styled_welcome_details(versions: &WolframVersions) {
    let accent = nu_ansi_term::Style::new()
        .bold()
        .fg(nu_ansi_term::Color::Rgb(250, 70, 35));
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
) -> Result<Option<String>> {
    let prompt = KernelInputPrompt {
        text: request.prompt.clone(),
        theme,
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
}

impl Prompt for KernelInputPrompt {
    fn render_prompt_left(&self) -> std::borrow::Cow<'_, str> {
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
