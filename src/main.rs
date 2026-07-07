use std::{
    collections::{HashMap, HashSet},
    env,
    error::Error,
    ffi::OsString,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use nu_ansi_term::{Color, Style};
use reedline::{
    Completer, EditCommand, Emacs, FileBackedHistory, Highlighter, IdeMenu, KeyCode, KeyModifiers,
    MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Span, StyledText, Suggestion, ValidationResult, Validator,
    default_emacs_keybindings,
};
use wolfram_app_discovery::WolframApp;

const COMPLETION_MENU: &str = "completion_menu";
const BUILTIN_SYMBOLS: &str = include_str!(concat!(env!("OUT_DIR"), "/builtin_symbols.tsv"));

type SharedKernel = Arc<Mutex<KernelClient>>;

#[derive(Debug)]
struct KernelExit {
    code: i32,
}

impl KernelExit {
    fn new(code: i32) -> Self {
        Self { code }
    }
}

impl fmt::Display for KernelExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "kernel requested process exit with code {}", self.code)
    }
}

impl Error for KernelExit {}

#[derive(Debug, Parser)]
#[command(name = "wolfram-cli")]
#[command(about = "A friendlier CLI interface for the Wolfram Kernel")]
struct Args {
    /// Disable Wolfram FrontEnd-backed completions and rendering support.
    #[arg(long = "no-frontend")]
    no_frontend: bool,

    /// Evaluate a Wolfram Language expression and exit.
    #[arg(short = 'e', long = "eval")]
    eval: Option<String>,

    /// Execute a Wolfram Language script or package file and exit.
    file: Option<PathBuf>,

    /// Arguments passed to the script file.
    #[arg(last = true)]
    script_args: Vec<OsString>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let result = match (args.eval, args.file) {
        (Some(expr), None) => KernelClient::new().evaluate_once(&expr),
        (None, Some(file)) => run_wolframscript_file(file, args.script_args),
        (None, None) => run_repl(!args.no_frontend),
        (Some(_), Some(_)) => bail!("use either --eval or a file, not both"),
    };

    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            if let Some(exit) = err.downcast_ref::<KernelExit>() {
                std::process::exit(exit.code);
            }
            Err(err)
        }
    }
}

fn run_repl(enable_frontend: bool) -> Result<()> {
    let history = history_path()?;
    let completion_epoch = Arc::new(AtomicU64::new(0));
    let kernel = Arc::new(Mutex::new(KernelClient::new()));
    let frontend = if enable_frontend {
        Some(Arc::new(Mutex::new(FrontEndClient::new())))
    } else {
        None
    };
    print_welcome(&kernel, frontend.as_ref())?;
    let theme = ThemeHandle::new(Theme::Dark);
    let completion_source =
        CompletionSource::new(kernel.clone(), frontend.clone(), completion_epoch.clone());
    let symbol_set = builtin_symbol_names().collect();
    let mut line_editor = Reedline::create()
        .use_kitty_keyboard_enhancement(true)
        .with_history(Box::new(FileBackedHistory::with_file(2000, history)?))
        .with_highlighter(Box::new(WolframHighlighter::new(symbol_set, theme.clone())))
        .with_completer(Box::new(WolframCompleter::new(
            completion_source,
            theme.clone(),
        )))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(completion_menu())))
        .with_validator(Box::new(WolframValidator))
        .with_edit_mode(Box::new(completion_edit_mode()));
    let mut line_number = 1;
    loop {
        let prompt = WolframPrompt {
            line_number,
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
                    if execute_repl_command(input, &theme) == CommandAction::Quit {
                        break;
                    }
                    continue;
                }
                lock_kernel(&kernel)?.evaluate_repl_input(input, line_number, &theme)?;
                completion_epoch.fetch_add(1, Ordering::Relaxed);
                line_number += 1;
            }
            Signal::CtrlC => continue,
            Signal::CtrlD => break,
        }
    }

    Ok(())
}

fn print_welcome(
    kernel: &SharedKernel,
    frontend: Option<&Arc<Mutex<FrontEndClient>>>,
) -> Result<()> {
    println!("\x1b[1;31mWolfram CLI\x1b[0m");
    println!(
        "Kernel: {} | FrontEnd: {}",
        kernel_status(kernel)?,
        frontend_status(frontend)?
    );
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!("Type :help for commands, :quit or Ctrl-D to quit.\n");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandAction {
    Continue,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplCommand {
    Help,
    Theme(ThemeCommand),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThemeCommand {
    Cycle,
    List,
    Show,
    Set(Theme),
}

fn execute_repl_command(input: &str, theme: &ThemeHandle) -> CommandAction {
    let command = match parse_repl_command(input) {
        Ok(command) => command,
        Err(message) => {
            println!("{message}");
            return CommandAction::Continue;
        }
    };

    match command {
        ReplCommand::Help => {
            print_command_help(theme.current());
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::Cycle) => {
            let next = theme.current().next();
            theme.set(next);
            println!("Theme: {}", next.name());
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::List) => {
            print_theme_browser(theme.current());
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::Show) => {
            println!("Theme: {}", theme.current().name());
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::Set(next)) => {
            theme.set(next);
            println!("Theme: {}", next.name());
            CommandAction::Continue
        }
        ReplCommand::Quit => CommandAction::Quit,
    }
}

fn print_command_help(theme: Theme) {
    println!("Commands:");
    println!("  :help                 Show this help.");
    println!(
        "  :theme                Cycle theme. Current: {}.",
        theme.name()
    );
    println!("  :theme dark|light|solarized|gruvbox|monokai|plain");
    println!("                       Set syntax highlighting theme.");
    println!("  :theme list          Browse available themes.");
    println!("  :theme show           Show the current theme.");
    println!("  :quit                 Quit the REPL.");
}

fn parse_repl_command(input: &str) -> Result<ReplCommand> {
    let command = input
        .trim()
        .strip_prefix(':')
        .context("commands start with ':'")?
        .trim();

    if command.is_empty() {
        bail!("empty command; type :help for commands");
    }

    let mut parts = command.split_whitespace();
    let name = parts.next().expect("command is not empty").to_lowercase();
    match name.as_str() {
        "help" | "h" | "?" => {
            reject_extra_args(parts, ":help")?;
            Ok(ReplCommand::Help)
        }
        "quit" | "q" | "exit" => {
            reject_extra_args(parts, ":quit")?;
            Ok(ReplCommand::Quit)
        }
        "theme" => match parts.next() {
            None => Ok(ReplCommand::Theme(ThemeCommand::Cycle)),
            Some("show" | "current") => {
                reject_extra_args(parts, ":theme show")?;
                Ok(ReplCommand::Theme(ThemeCommand::Show))
            }
            Some("list" | "ls" | "browse") => {
                reject_extra_args(parts, ":theme list")?;
                Ok(ReplCommand::Theme(ThemeCommand::List))
            }
            Some(value) => {
                let theme_value = value.to_lowercase();
                let theme = Theme::parse(&theme_value)
                    .with_context(|| format!("unknown theme {value:?}; use :theme list"))?;
                reject_extra_args(parts, ":theme <name>")?;
                Ok(ReplCommand::Theme(ThemeCommand::Set(theme)))
            }
        },
        _ => bail!("unknown command :{name}; type :help for commands"),
    }
}

fn reject_extra_args(mut parts: std::str::SplitWhitespace<'_>, usage: &str) -> Result<()> {
    if parts.next().is_some() {
        bail!("usage: {usage}");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Theme {
    Dark = 0,
    Light = 1,
    Solarized = 2,
    Gruvbox = 3,
    Monokai = 4,
    Plain = 5,
}

impl Theme {
    const ALL: [Self; 6] = [
        Self::Dark,
        Self::Light,
        Self::Solarized,
        Self::Gruvbox,
        Self::Monokai,
        Self::Plain,
    ];

    fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "solarized" | "solarized-dark" => Some(Self::Solarized),
            "gruvbox" | "gruvbox-dark" => Some(Self::Gruvbox),
            "monokai" => Some(Self::Monokai),
            "plain" | "none" | "no-color" | "nocolor" => Some(Self::Plain),
            _ => None,
        }
    }

    fn from_id(value: u8) -> Self {
        match value {
            1 => Self::Light,
            2 => Self::Solarized,
            3 => Self::Gruvbox,
            4 => Self::Monokai,
            5 => Self::Plain,
            _ => Self::Dark,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized",
            Self::Gruvbox => "gruvbox",
            Self::Monokai => "monokai",
            Self::Plain => "plain",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Solarized,
            Self::Solarized => Self::Gruvbox,
            Self::Gruvbox => Self::Monokai,
            Self::Monokai => Self::Plain,
            Self::Plain => Self::Dark,
        }
    }

    fn styles(self) -> ThemeStyles {
        match self {
            Self::Dark => ThemeStyles {
                string: Style::new().fg(Color::Green),
                comment: Style::new().fg(Color::DarkGray),
                number: Style::new().fg(Color::Purple),
                builtin_symbol: Style::new().fg(Color::Cyan).bold(),
                completion_command: Style::new().fg(Color::LightBlue),
                completion_symbol: Style::new().fg(Color::Cyan),
                completion_context: Style::new().fg(Color::Yellow),
                completion_option: Style::new().fg(Color::Yellow),
                completion_argument: Style::new().fg(Color::Magenta),
                completion_color: Style::new().fg(Color::LightRed),
                completion_file: Style::new().fg(Color::LightBlue),
                completion_directory: Style::new().fg(Color::LightGreen),
            },
            Self::Light => ThemeStyles {
                string: Style::new().fg(Color::Fixed(28)),
                comment: Style::new().fg(Color::Fixed(244)),
                number: Style::new().fg(Color::Fixed(90)),
                builtin_symbol: Style::new().fg(Color::Fixed(25)).bold(),
                completion_command: Style::new().fg(Color::Fixed(25)),
                completion_symbol: Style::new().fg(Color::Fixed(25)),
                completion_context: Style::new().fg(Color::Fixed(130)),
                completion_option: Style::new().fg(Color::Fixed(130)),
                completion_argument: Style::new().fg(Color::Fixed(90)),
                completion_color: Style::new().fg(Color::Fixed(160)),
                completion_file: Style::new().fg(Color::Fixed(31)),
                completion_directory: Style::new().fg(Color::Fixed(28)),
            },
            Self::Solarized => ThemeStyles {
                string: Style::new().fg(Color::Fixed(64)),
                comment: Style::new().fg(Color::Fixed(244)).italic(),
                number: Style::new().fg(Color::Fixed(136)),
                builtin_symbol: Style::new().fg(Color::Fixed(33)).bold(),
                completion_command: Style::new().fg(Color::Fixed(33)),
                completion_symbol: Style::new().fg(Color::Fixed(33)),
                completion_context: Style::new().fg(Color::Fixed(136)),
                completion_option: Style::new().fg(Color::Fixed(136)),
                completion_argument: Style::new().fg(Color::Fixed(125)),
                completion_color: Style::new().fg(Color::Fixed(160)),
                completion_file: Style::new().fg(Color::Fixed(37)),
                completion_directory: Style::new().fg(Color::Fixed(64)),
            },
            Self::Gruvbox => ThemeStyles {
                string: Style::new().fg(Color::Fixed(142)),
                comment: Style::new().fg(Color::Fixed(245)).italic(),
                number: Style::new().fg(Color::Fixed(208)),
                builtin_symbol: Style::new().fg(Color::Fixed(109)).bold(),
                completion_command: Style::new().fg(Color::Fixed(109)),
                completion_symbol: Style::new().fg(Color::Fixed(109)),
                completion_context: Style::new().fg(Color::Fixed(214)),
                completion_option: Style::new().fg(Color::Fixed(214)),
                completion_argument: Style::new().fg(Color::Fixed(175)),
                completion_color: Style::new().fg(Color::Fixed(167)),
                completion_file: Style::new().fg(Color::Fixed(109)),
                completion_directory: Style::new().fg(Color::Fixed(142)),
            },
            Self::Monokai => ThemeStyles {
                string: Style::new().fg(Color::Fixed(148)),
                comment: Style::new().fg(Color::Fixed(59)).italic(),
                number: Style::new().fg(Color::Fixed(141)),
                builtin_symbol: Style::new().fg(Color::Fixed(81)).bold(),
                completion_command: Style::new().fg(Color::Fixed(81)),
                completion_symbol: Style::new().fg(Color::Fixed(81)),
                completion_context: Style::new().fg(Color::Fixed(186)),
                completion_option: Style::new().fg(Color::Fixed(186)),
                completion_argument: Style::new().fg(Color::Fixed(197)),
                completion_color: Style::new().fg(Color::Fixed(197)),
                completion_file: Style::new().fg(Color::Fixed(81)),
                completion_directory: Style::new().fg(Color::Fixed(148)),
            },
            Self::Plain => ThemeStyles {
                string: Style::new(),
                comment: Style::new(),
                number: Style::new(),
                builtin_symbol: Style::new(),
                completion_command: Style::new(),
                completion_symbol: Style::new(),
                completion_context: Style::new(),
                completion_option: Style::new(),
                completion_argument: Style::new(),
                completion_color: Style::new(),
                completion_file: Style::new(),
                completion_directory: Style::new(),
            },
        }
    }
}

fn print_theme_browser(current: Theme) {
    println!("Themes:");
    for theme in Theme::ALL {
        let marker = if theme == current { "*" } else { " " };
        print!("  {marker} {:<9} ", theme.name());
        print_highlighted("Plot[Sin[x], {x, 0, 2 Pi}] (* sample *) \"text\" 42", theme);
    }
}

fn print_highlighted(text: &str, theme: Theme) {
    for (style, fragment) in highlight_wolfram_text(text, theme.styles(), None).buffer {
        print!("{}", style.paint(fragment));
    }
    println!();
}

#[derive(Clone)]
struct ThemeHandle {
    current: Arc<AtomicU64>,
}

impl ThemeHandle {
    fn new(theme: Theme) -> Self {
        Self {
            current: Arc::new(AtomicU64::new(theme as u64)),
        }
    }

    fn current(&self) -> Theme {
        Theme::from_id(self.current.load(Ordering::Relaxed) as u8)
    }

    fn set(&self, theme: Theme) {
        self.current.store(theme as u64, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct ThemeStyles {
    string: Style,
    comment: Style,
    number: Style,
    builtin_symbol: Style,
    completion_command: Style,
    completion_symbol: Style,
    completion_context: Style,
    completion_option: Style,
    completion_argument: Style,
    completion_color: Style,
    completion_file: Style,
    completion_directory: Style,
}

fn kernel_status(kernel: &SharedKernel) -> Result<KernelStatus> {
    match kernel.try_lock() {
        Ok(kernel) => Ok(kernel.status()),
        Err(std::sync::TryLockError::WouldBlock) => Ok(KernelStatus::Active),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            Err(anyhow!("kernel session lock was poisoned"))
        }
    }
}

fn frontend_status(frontend: Option<&Arc<Mutex<FrontEndClient>>>) -> Result<FrontEndStatus> {
    let Some(frontend) = frontend else {
        return Ok(FrontEndStatus::Disabled);
    };

    match frontend.try_lock() {
        Ok(frontend) => Ok(frontend.status()),
        Err(std::sync::TryLockError::WouldBlock) => Ok(FrontEndStatus::Active),
        Err(std::sync::TryLockError::Poisoned(_)) => Err(anyhow!("FrontEnd lock was poisoned")),
    }
}

struct WolframPrompt {
    line_number: usize,
    kernel_status: KernelStatus,
    frontend_status: FrontEndStatus,
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

fn completion_menu() -> IdeMenu {
    IdeMenu::default()
        .with_name(COMPLETION_MENU)
        .with_only_buffer_difference(false)
        .with_max_completion_height(6)
        .with_max_description_height(6)
}

fn completion_edit_mode() -> Emacs {
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

    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('$'),
        insert_and_open_completion('$'),
    );
    keybindings.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::Char('$'),
        insert_and_open_completion('$'),
    );
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('`'),
        insert_and_open_completion('`'),
    );

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

struct WolframValidator;

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

fn wolfram_input_is_incomplete(line: &str) -> bool {
    let mut stack = Vec::new();
    let mut chars = line.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    let mut comment_depth = 0usize;

    while let Some(ch) = chars.next() {
        if comment_depth > 0 {
            match ch {
                '(' if chars.peek() == Some(&'*') => {
                    chars.next();
                    comment_depth += 1;
                }
                '*' if chars.peek() == Some(&')') => {
                    chars.next();
                    comment_depth -= 1;
                }
                _ => {}
            }
            continue;
        }

        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '(' if chars.peek() == Some(&'*') => {
                chars.next();
                comment_depth = 1;
            }
            '[' | '{' | '(' => stack.push(ch),
            ']' => {
                if !matches!(stack.pop(), Some('[')) {
                    return false;
                }
            }
            '}' => {
                if !matches!(stack.pop(), Some('{')) {
                    return false;
                }
            }
            ')' => {
                if !matches!(stack.pop(), Some('(')) {
                    return false;
                }
            }
            _ => {}
        }
    }

    in_string || comment_depth > 0 || !stack.is_empty()
}

fn lock_kernel(kernel: &SharedKernel) -> Result<std::sync::MutexGuard<'_, KernelClient>> {
    kernel
        .lock()
        .map_err(|_| anyhow!("kernel session lock was poisoned"))
}

fn history_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .context("could not determine a history directory")?;
    let dir = base.join("wolfram-cli");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("history"))
}

fn run_wolframscript_file(file: PathBuf, script_args: Vec<OsString>) -> Result<()> {
    let status = Command::new("wolframscript")
        .arg("-file")
        .arg(file)
        .args(script_args)
        .status()
        .context("failed to launch wolframscript")?;

    if !status.success() {
        if let Some(code) = status.code() {
            return Err(KernelExit::new(code).into());
        }
        bail!("wolframscript exited with {status}");
    }
    Ok(())
}

struct KernelClient {
    #[cfg(feature = "wstp")]
    wstp: Option<native_wstp::WstpKernelClient>,
    active: Arc<AtomicBool>,
    #[cfg(feature = "wstp")]
    ready: bool,
}

#[derive(Clone, Copy)]
enum KernelStatus {
    Active,
    #[cfg(feature = "wstp")]
    StartingWstp,
    #[cfg(feature = "wstp")]
    ReadyWstp,
    Subprocess,
}

impl fmt::Display for KernelStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            #[cfg(feature = "wstp")]
            Self::StartingWstp => f.write_str("starting/WSTP"),
            #[cfg(feature = "wstp")]
            Self::ReadyWstp => f.write_str("ready/WSTP"),
            Self::Subprocess => f.write_str("subprocess"),
        }
    }
}

struct ActivityGuard {
    active: Arc<AtomicBool>,
}

impl ActivityGuard {
    fn new(active: Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Relaxed);
        Self { active }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

impl KernelClient {
    fn new() -> Self {
        let active = Arc::new(AtomicBool::new(false));
        #[cfg(feature = "wstp")]
        {
            let wstp = match native_wstp::WstpKernelClient::launch() {
                Ok(client) => Some(client),
                Err(err) => {
                    eprintln!("warning: WSTP backend unavailable, using subprocess mode: {err:#}");
                    None
                }
            };
            Self {
                wstp,
                active,
                ready: false,
            }
        }

        #[cfg(not(feature = "wstp"))]
        {
            Self { active }
        }
    }

    fn evaluate_once(&mut self, input: &str) -> Result<()> {
        self.evaluate(input, None)
    }

    fn status(&self) -> KernelStatus {
        if self.active.load(Ordering::Relaxed) {
            return KernelStatus::Active;
        }

        #[cfg(feature = "wstp")]
        {
            if self.wstp.is_some() {
                return if self.ready {
                    KernelStatus::ReadyWstp
                } else {
                    KernelStatus::StartingWstp
                };
            }
        }

        KernelStatus::Subprocess
    }

    fn evaluate_repl_input(
        &mut self,
        input: &str,
        line_number: usize,
        theme: &ThemeHandle,
    ) -> Result<()> {
        self.evaluate(input, Some((line_number, theme)))
    }

    fn evaluate(&mut self, input: &str, line_number: Option<(usize, &ThemeHandle)>) -> Result<()> {
        let _activity = ActivityGuard::new(self.active.clone());

        #[cfg(feature = "wstp")]
        if let Some(wstp) = &mut self.wstp {
            if let Err(err) = wstp.evaluate_once(input, line_number) {
                if err.downcast_ref::<KernelExit>().is_some() {
                    return Err(err);
                }
                eprintln!("warning: WSTP evaluation failed, using subprocess mode: {err:#}");
                self.wstp = None;
                self.ready = false;
            } else {
                self.ready = true;
                return Ok(());
            }
        }

        evaluate_with_subprocess(input, line_number)
    }

    fn query_lines(&mut self, code: &str) -> Result<Vec<String>> {
        let output = self.query_string(code)?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }

    fn query_string(&mut self, code: &str) -> Result<String> {
        let _activity = ActivityGuard::new(self.active.clone());

        #[cfg(feature = "wstp")]
        if let Some(wstp) = &mut self.wstp {
            match wstp.evaluate_to_string(code) {
                Ok(output) => {
                    self.ready = true;
                    return Ok(output);
                }
                Err(err) => {
                    eprintln!(
                        "warning: WSTP completion query failed, using subprocess mode: {err:#}"
                    );
                    self.wstp = None;
                    self.ready = false;
                }
            }
        }

        query_string_with_subprocess(code)
    }
}

fn evaluate_with_subprocess(input: &str, line_number: Option<(usize, &ThemeHandle)>) -> Result<()> {
    let code = format!(
        "WriteString[$Output, ToString[ToExpression[{}], InputForm], \"\\n\"]",
        wolfram_string_literal(input)
    );
    let output = Command::new(kernel_path()?)
        .arg("-noprompt")
        .arg("-run")
        .arg(format!("{code}; Quit[]"))
        .output()
        .context("failed to launch WolframKernel")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some((line_number, theme)) = line_number.filter(|_| !stdout.is_empty()) {
        print!("Out[{line_number}]= ");
        print_highlighted(stdout.trim_end_matches('\n'), theme.current());
    } else {
        print!("{stdout}");
    }
    eprint!("{}", String::from_utf8_lossy(&output.stderr));

    if !output.status.success() {
        if let Some(code) = output.status.code() {
            return Err(KernelExit::new(code).into());
        }
        bail!("WolframKernel exited with {}", output.status);
    }
    Ok(())
}

#[cfg(test)]
fn strip_evaluation_complete_marker<'a>(stdout: &'a str, marker: &str) -> Option<&'a str> {
    stdout.strip_suffix(marker)
}

fn query_string_with_subprocess(code: &str) -> Result<String> {
    let output = Command::new(kernel_path()?)
        .arg("-noprompt")
        .arg("-run")
        .arg(format!(
            "WriteString[$Output, ToString[{}, OutputForm]]; Quit[]",
            code
        ))
        .output()
        .context("failed to launch WolframKernel for completion query")?;

    if !output.status.success() {
        bail!(
            "WolframKernel completion query exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout).context("WolframKernel returned invalid UTF-8")
}

fn kernel_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("WOLFRAM_KERNEL") {
        return Ok(PathBuf::from(path));
    }

    if let Ok(install_dir) = wolfram_installation_directory() {
        if let Some(candidate) = native_kernel_path(&install_dir) {
            if candidate.exists() {
                return Ok(candidate);
            }
        }

        let candidate = install_dir.join("Executables").join(kernel_binary_name());
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Ok(PathBuf::from(kernel_binary_name()))
}

fn native_kernel_path(install_dir: &Path) -> Option<PathBuf> {
    let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "Linux-x86-64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "Linux-ARM64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "MacOSX-x86-64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "MacOSX-ARM64"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "Windows-x86-64"
    } else {
        return None;
    };

    Some(
        install_dir
            .join("SystemFiles")
            .join("Kernel")
            .join("Binaries")
            .join(platform)
            .join(kernel_binary_name()),
    )
}

fn kernel_binary_name() -> &'static str {
    if cfg!(windows) {
        "WolframKernel.exe"
    } else {
        "WolframKernel"
    }
}

fn wolfram_installation_directory() -> Result<PathBuf> {
    if let Ok(path) = wolframscript_showkernels_installation_directory() {
        return Ok(path);
    }

    WolframApp::try_default()
        .map(|app| app.installation_directory())
        .context("failed to discover Wolfram installation")
}

fn wolframscript_showkernels_installation_directory() -> Result<PathBuf> {
    let output = Command::new("wolframscript")
        .arg("-showkernels")
        .output()
        .context("failed to launch wolframscript for installation discovery")?;

    if !output.status.success() {
        bail!(
            "wolframscript installation discovery exited with {}",
            output.status
        );
    }

    let stdout =
        String::from_utf8(output.stdout).context("wolframscript returned invalid UTF-8")?;
    for line in stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let path = PathBuf::from(line);
        if path
            .file_name()
            .is_some_and(|name| name == kernel_binary_name())
            && path.parent().and_then(Path::file_name) == Some("Executables".as_ref())
        {
            return path
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .context("wolframscript returned a kernel path without an installation directory");
        }
    }

    bail!("wolframscript -showkernels did not return a WolframKernel path")
}

struct FrontEndClient {
    install_dir: Option<PathBuf>,
    unavailable: bool,
    active: bool,
}

#[derive(Clone, Copy)]
enum FrontEndStatus {
    Active,
    Lazy,
    Ready,
    Disabled,
    Unavailable,
}

impl fmt::Display for FrontEndStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Lazy => f.write_str("lazy"),
            Self::Ready => f.write_str("ready"),
            Self::Disabled => f.write_str("disabled"),
            Self::Unavailable => f.write_str("unavailable"),
        }
    }
}

impl FrontEndClient {
    fn new() -> Self {
        Self {
            install_dir: None,
            unavailable: false,
            active: false,
        }
    }

    fn status(&self) -> FrontEndStatus {
        if self.active {
            FrontEndStatus::Active
        } else if self.unavailable {
            FrontEndStatus::Unavailable
        } else if self.install_dir.is_some() {
            FrontEndStatus::Ready
        } else {
            FrontEndStatus::Lazy
        }
    }

    fn installation_directory(&mut self) -> Result<Option<PathBuf>> {
        if self.unavailable {
            return Ok(None);
        }

        if let Some(install_dir) = &self.install_dir {
            return Ok(Some(install_dir.clone()));
        }

        self.set_active(true);
        let init = (|| {
            let install_dir = wolfram_installation_directory()
                .context("install wolframscript for FrontEnd completion discovery")?;
            let _path = frontend_path(&install_dir)?;
            query_string_with_subprocess(
                "UsingFrontEnd[Quiet[Check[ToString[$FrontEnd =!= $Failed, OutputForm], \"False\"]]]",
            )
            .context("failed to initialize Wolfram FrontEnd in the background")?;
            Ok::<_, anyhow::Error>(install_dir)
        })();
        self.set_active(false);

        let install_dir = match init {
            Ok(install_dir) => install_dir,
            Err(err) => {
                self.unavailable = true;
                return Err(err);
            }
        };
        self.install_dir = Some(install_dir.clone());
        Ok(Some(install_dir))
    }

    fn load_argument_completions(
        &mut self,
        head: &str,
        argument_index: usize,
    ) -> Result<Vec<ArgumentCompletionItem>> {
        let Some(install_dir) = self.installation_directory()? else {
            return Ok(Vec::new());
        };

        let code = frontend_argument_completion_query(&install_dir, head, argument_index);
        self.set_active(true);
        let output = query_string_with_subprocess(&code)
            .with_context(|| format!("failed to query FrontEnd argument completions for {head}"));
        self.set_active(false);
        let output = output?;
        Ok(parse_argument_completion_items(&output))
    }

    fn set_active(&mut self, active: bool) {
        self.active = active;
    }
}

fn frontend_path(install_dir: &Path) -> Result<PathBuf> {
    if let Some(path) = env::var_os("WOLFRAM_FRONTEND") {
        return Ok(PathBuf::from(path));
    }

    frontend_candidates(install_dir)
        .into_iter()
        .find(|candidate| candidate.exists())
        .with_context(|| {
            format!(
                "could not find Wolfram FrontEnd under {}",
                install_dir.display()
            )
        })
}

fn frontend_candidates(install_dir: &Path) -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            install_dir.join("Contents").join("MacOS").join("Wolfram"),
            install_dir.join("Executables").join("Wolfram"),
            install_dir.join("Executables").join("Mathematica"),
        ]
    } else if cfg!(windows) {
        vec![
            install_dir.join("Wolfram.exe"),
            install_dir.join("Executables").join("Wolfram.exe"),
            install_dir.join("Mathematica.exe"),
            install_dir.join("Executables").join("Mathematica.exe"),
        ]
    } else {
        vec![
            install_dir.join("Executables").join("WolframNB"),
            install_dir.join("Executables").join("Wolfram"),
            install_dir.join("Executables").join("Mathematica"),
            install_dir.join("WolframNB"),
            install_dir.join("Wolfram"),
            install_dir.join("Mathematica"),
        ]
    }
}

fn frontend_argument_completion_query(
    install_dir: &Path,
    head: &str,
    argument_index: usize,
) -> String {
    let install_dir = wolfram_string_literal(&install_dir.display().to_string());
    let head = wolfram_string_literal(head);
    format!(
        r#"Module[{{install = {install_dir}, target = {head}, index = {argument_index}, files, held, rules, matched}},
  files = Quiet @ Select[
    FileNames[{{"*.m", "*.wl", "*.tr"}}, install, Infinity],
    Quiet[FileByteCount[#] < 2000000 && StringContainsQ[Import[#, "Text"], "AddSpecialArgCompletion"]]&
  ];
  held = Reap[
    Block[{{FEPrivate`AddSpecialArgCompletion = Function[args, Sow[HoldComplete[args]], HoldAll]}},
      Scan[
        Quiet[Check[ToExpression[Import[#, "Text"], InputForm, HoldComplete], Null]]&,
        files
      ]
    ]
  ][[2]];
  rules = Cases[held, HoldComplete[args___] :> HoldComplete[args], Infinity];
  matched = Cases[
    rules,
    HoldComplete[(s_String | HoldPattern[Alternatives[___, s_String, ___]]), completions_] /; s == target :> completions,
    Infinity
  ];
  matched = Cases[matched, s_String :> s, Infinity];
  matched = DeleteDuplicates[Join[
    If[MemberQ[matched, "Color" | "ColorSetter" | "ColorSlider"], ("color\t" <> #)& /@ ColorData["HTML", "ColorNames"], {{}}],
    If[MemberQ[matched, "FileName" | "File" | "OpenFile" | "SaveFile"], {{"file\t./"}}, {{}}],
    If[MemberQ[matched, "Directory" | "DirectoryName" | "Folder"], {{"directory\t./"}}, {{}}],
    ("value\t" <> #)& /@ matched
  ]];
  StringRiffle[Take[matched, UpTo[200]], "\n"]
]"#
    )
}

#[derive(Clone)]
/// A cached value tagged with the completion epoch it was produced under, so a
/// stale in-flight result (e.g. the kernel gained new definitions while a
/// background fetch was running) is detected by comparison rather than needing
/// coordinated cache invalidation.
enum CacheEntry<V> {
    Pending(u64),
    Ready(u64, V),
}

enum CachePoll<V> {
    Ready(V),
    Pending,
    /// No fresh entry exists; the caller just claimed it (marked `Pending`) and
    /// is responsible for spawning a background fetch.
    Spawn,
}

/// A `Mutex`-backed cache shared between the input thread (reader) and the
/// completion worker thread (writer). Reads and writes are both just a brief
/// lock of an in-memory map, so callers on the input thread never block on IO.
struct AsyncCache<K, V> {
    entries: Arc<Mutex<HashMap<K, CacheEntry<V>>>>,
}

impl<K, V> Clone for AsyncCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            entries: Arc::clone(&self.entries),
        }
    }
}

impl<K: Eq + std::hash::Hash + Clone, V: Clone> AsyncCache<K, V> {
    fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn poll_or_claim(&self, key: &K, epoch: u64) -> CachePoll<V> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match entries.get(key) {
            Some(CacheEntry::Ready(entry_epoch, value)) if *entry_epoch == epoch => {
                CachePoll::Ready(value.clone())
            }
            Some(CacheEntry::Pending(entry_epoch)) if *entry_epoch == epoch => CachePoll::Pending,
            _ => {
                entries.insert(key.clone(), CacheEntry::Pending(epoch));
                CachePoll::Spawn
            }
        }
    }

    fn insert(&self, key: K, epoch: u64, value: V) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.insert(key, CacheEntry::Ready(epoch, value));
    }
}

/// The blocking, kernel-touching half of completion. Calls here can take
/// anywhere from a few milliseconds to multiple seconds (rendering a usage
/// message for many symbols at once is the expensive case), so this trait must
/// only ever be driven from the background completion worker thread, never
/// from `Completer::complete` on reedline's input thread.
trait KernelBackend: Send + Sync {
    fn load_symbols_for_prefix(&self, prefix: &str) -> Result<Vec<CompletionItem>>;
    fn load_symbol_details(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, CompletionItemDetails>>;
    fn load_options(&self, head: &str) -> Result<Vec<String>>;
    fn load_argument_completions(
        &self,
        head: &str,
        argument_index: usize,
    ) -> Result<Vec<ArgumentCompletionItem>>;
}

struct KernelBackendImpl {
    kernel: SharedKernel,
    frontend: Option<Arc<Mutex<FrontEndClient>>>,
}

impl KernelBackendImpl {
    fn query_lines(&self, code: &str) -> Result<Vec<String>> {
        lock_kernel(&self.kernel)?.query_lines(code)
    }
}

impl KernelBackend for KernelBackendImpl {
    fn load_symbols_for_prefix(&self, prefix: &str) -> Result<Vec<CompletionItem>> {
        let code = symbol_completion_query(prefix);
        let lines = self
            .query_lines(&code)
            .with_context(|| format!("failed to load names for prefix {prefix:?}"))?;
        Ok(parse_completion_items(lines))
    }

    fn load_symbol_details(
        &self,
        symbols: &[String],
    ) -> Result<HashMap<String, CompletionItemDetails>> {
        let code = symbol_details_batch_query(symbols);
        let lines = self
            .query_lines(&code)
            .context("failed to load symbol usage batch")?;
        Ok(parse_symbol_details_batch(lines))
    }

    fn load_options(&self, head: &str) -> Result<Vec<String>> {
        let literal = wolfram_string_literal(head);
        let code = format!(
            "Quiet[Check[StringRiffle[ToString /@ First /@ Options[ToExpression[{literal}]], \"\\n\"], \"\"]]"
        );
        self.query_lines(&code)
            .with_context(|| format!("failed to load options for {head}"))
    }

    fn load_argument_completions(
        &self,
        head: &str,
        argument_index: usize,
    ) -> Result<Vec<ArgumentCompletionItem>> {
        let Some(frontend) = &self.frontend else {
            return Ok(Vec::new());
        };

        frontend
            .lock()
            .map_err(|_| anyhow!("FrontEnd lock was poisoned"))?
            .load_argument_completions(head, argument_index)
    }
}

enum CompletionJob {
    Symbols {
        prefix: String,
        epoch: u64,
    },
    Details {
        symbols: Vec<String>,
        epoch: u64,
    },
    Options {
        head: String,
        epoch: u64,
    },
    Arguments {
        head: String,
        argument_index: usize,
        epoch: u64,
    },
}

fn spawn_completion_worker(
    backend: Arc<dyn KernelBackend>,
    symbols_cache: AsyncCache<String, Vec<CompletionItem>>,
    details_cache: AsyncCache<String, CompletionItemDetails>,
    options_cache: AsyncCache<String, Vec<String>>,
    arguments_cache: AsyncCache<(String, usize), Vec<ArgumentCompletionItem>>,
) -> mpsc::Sender<CompletionJob> {
    let (sender, receiver) = mpsc::channel::<CompletionJob>();
    thread::spawn(move || {
        for job in receiver {
            match job {
                CompletionJob::Symbols { prefix, epoch } => {
                    let items = backend
                        .load_symbols_for_prefix(&prefix)
                        .unwrap_or_else(|err| {
                            eprintln!(
                                "warning: symbol completion disabled for {prefix:?}: {err:#}"
                            );
                            Vec::new()
                        });
                    symbols_cache.insert(prefix, epoch, items);
                }
                CompletionJob::Details { symbols, epoch } => match backend
                    .load_symbol_details(&symbols)
                {
                    Ok(mut details) => {
                        for symbol in symbols {
                            let entry = details.remove(&symbol).unwrap_or(CompletionItemDetails {
                                context: None,
                                usage: None,
                            });
                            details_cache.insert(symbol, epoch, entry);
                        }
                    }
                    Err(err) => {
                        eprintln!("warning: symbol details disabled: {err:#}");
                        for symbol in symbols {
                            details_cache.insert(
                                symbol,
                                epoch,
                                CompletionItemDetails {
                                    context: None,
                                    usage: None,
                                },
                            );
                        }
                    }
                },
                CompletionJob::Options { head, epoch } => {
                    let options = backend.load_options(&head).unwrap_or_else(|err| {
                        eprintln!("warning: option completion disabled for {head}: {err:#}");
                        Vec::new()
                    });
                    options_cache.insert(head, epoch, options);
                }
                CompletionJob::Arguments {
                    head,
                    argument_index,
                    epoch,
                } => {
                    let completions = backend
                        .load_argument_completions(&head, argument_index)
                        .unwrap_or_else(|err| {
                            eprintln!(
                                "warning: FrontEnd argument completion disabled for {head}: {err:#}"
                            );
                            Vec::new()
                        });
                    arguments_cache.insert((head, argument_index), epoch, completions);
                }
            }
        }
    });
    sender
}

struct CompletionSource {
    epoch: Arc<AtomicU64>,
    job_sender: mpsc::Sender<CompletionJob>,
    symbols_cache: AsyncCache<String, Vec<CompletionItem>>,
    details_cache: AsyncCache<String, CompletionItemDetails>,
    options_cache: AsyncCache<String, Vec<String>>,
    arguments_cache: AsyncCache<(String, usize), Vec<ArgumentCompletionItem>>,
}

impl CompletionSource {
    fn new(
        kernel: SharedKernel,
        frontend: Option<Arc<Mutex<FrontEndClient>>>,
        epoch: Arc<AtomicU64>,
    ) -> Self {
        Self::with_backend(Arc::new(KernelBackendImpl { kernel, frontend }), epoch)
    }

    fn with_backend(backend: Arc<dyn KernelBackend>, epoch: Arc<AtomicU64>) -> Self {
        let symbols_cache = AsyncCache::new();
        let details_cache = AsyncCache::new();
        let options_cache = AsyncCache::new();
        let arguments_cache = AsyncCache::new();
        let job_sender = spawn_completion_worker(
            backend,
            symbols_cache.clone(),
            details_cache.clone(),
            options_cache.clone(),
            arguments_cache.clone(),
        );
        Self {
            epoch,
            job_sender,
            symbols_cache,
            details_cache,
            options_cache,
            arguments_cache,
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Never touches the kernel directly. Built-ins resolve locally and
    /// instantly; kernel-sourced names for a not-yet-seen prefix are fetched on
    /// the background worker and simply aren't included until a later call
    /// (typically the next keystroke) finds them cached.
    fn symbols_for_prefix(&self, prefix: &str) -> Vec<CompletionItem> {
        if !is_qualified_symbol_name(prefix) {
            return Vec::new();
        }

        if prefix.starts_with("System`") {
            return builtin_symbols_for_prefix(prefix);
        }

        let epoch = self.epoch();
        let mut items = match self.symbols_cache.poll_or_claim(&prefix.to_string(), epoch) {
            CachePoll::Ready(items) => items,
            CachePoll::Pending => Vec::new(),
            CachePoll::Spawn => {
                let _ = self.job_sender.send(CompletionJob::Symbols {
                    prefix: prefix.to_string(),
                    epoch,
                });
                Vec::new()
            }
        };

        if !prefix.contains('`') {
            items.extend(builtin_symbols_for_prefix(prefix));
        }

        items
    }

    /// Returns whichever of `symbols` already have cached usage/context info;
    /// queues a single batched background fetch for the rest. Keep `symbols`
    /// bounded: it becomes one kernel round trip, and rendering usage messages
    /// is the single most expensive thing this program asks the kernel to do.
    fn usage_details(&self, symbols: &[String]) -> HashMap<String, CompletionItemDetails> {
        let epoch = self.epoch();
        let mut ready = HashMap::new();
        let mut to_spawn = Vec::new();
        for symbol in symbols {
            match self.details_cache.poll_or_claim(symbol, epoch) {
                CachePoll::Ready(details) => {
                    ready.insert(symbol.clone(), details);
                }
                CachePoll::Pending => {}
                CachePoll::Spawn => to_spawn.push(symbol.clone()),
            }
        }

        if !to_spawn.is_empty() {
            let _ = self.job_sender.send(CompletionJob::Details {
                symbols: to_spawn,
                epoch,
            });
        }

        ready
    }

    fn options_for(&self, head: &str) -> Vec<String> {
        if !is_qualified_symbol_name(head) {
            return Vec::new();
        }

        let epoch = self.epoch();
        match self.options_cache.poll_or_claim(&head.to_string(), epoch) {
            CachePoll::Ready(options) => options,
            CachePoll::Pending => Vec::new(),
            CachePoll::Spawn => {
                let _ = self.job_sender.send(CompletionJob::Options {
                    head: head.to_string(),
                    epoch,
                });
                Vec::new()
            }
        }
    }

    fn arguments_for(&self, head: &str, argument_index: usize) -> Vec<ArgumentCompletionItem> {
        let epoch = self.epoch();
        let key = (head.to_string(), argument_index);
        match self.arguments_cache.poll_or_claim(&key, epoch) {
            CachePoll::Ready(completions) => completions,
            CachePoll::Pending => Vec::new(),
            CachePoll::Spawn => {
                let _ = self.job_sender.send(CompletionJob::Arguments {
                    head: head.to_string(),
                    argument_index,
                    epoch,
                });
                Vec::new()
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionItem {
    value: String,
    kind: CompletionKind,
    frequency: Option<usize>,
    context: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArgumentCompletionItem {
    value: String,
    kind: ArgumentCompletionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArgumentCompletionKind {
    Value,
    Color,
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionKind {
    Symbol,
    Context,
}

/// Lists candidate names (and their context) for `prefix`. Deliberately does
/// NOT compute usage messages here: rendering `::usage` text for every match
/// is the expensive part of a completion query (~40ms/symbol for a query
/// spanning many symbols, vs ~1ms/symbol for name+context alone), so it would
/// turn a query matching dozens of names into a multi-second call. Usage text
/// is fetched separately, in small batches, via `symbol_details_batch_query`.
fn symbol_completion_query(prefix: &str) -> String {
    let prefix = wolfram_string_literal(prefix);
    let context_expr = wolfram_context_text_expr();
    format!(
        "With[{{p = {prefix}}}, Module[{{base, symbols, contexts, item}}, base = If[StringEndsQ[p, \"`\"], p, p]; symbols = Names[base <> \"*\"]; contexts = Select[Contexts[], StringStartsQ[#, base] &]; item[name_] := Module[{{sym = ToExpression[name, InputForm, HoldComplete], ctx}}, ctx = {context_expr}; StringRiffle[{{\"symbol\", name, \"0\", ctx}}, \"\\t\"]]; StringRiffle[Take[DeleteDuplicates[Join[item /@ symbols, (\"context\\t\" <> # <> \"\\t0\\t\" <> #) & /@ contexts]], UpTo[500]], \"\\n\"]]]"
    )
}

/// Fetches context + usage for a small, explicit list of symbol names in a
/// single kernel round trip (as opposed to one round trip per symbol).
fn symbol_details_batch_query(symbols: &[String]) -> String {
    let context_expr = wolfram_context_text_expr();
    let usage_expr = wolfram_usage_text_expr("ReleaseHold[sym]");
    let names = symbols
        .iter()
        .map(|symbol| wolfram_string_literal(symbol))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Module[{{names = {{{names}}}, item}}, item[name_] := Module[{{sym = ToExpression[name, InputForm, HoldComplete], ctx, usage}}, ctx = {context_expr}; usage = {usage_expr}; StringRiffle[{{name, ctx, usage}}, \"\\t\"]]; StringRiffle[item /@ names, \"\\n\"]]"
    )
}

fn wolfram_context_text_expr() -> &'static str {
    "Quiet[Check[ToString[Context @@ sym, OutputForm], \"\"]]"
}

fn wolfram_usage_text_expr(symbol_expr: &str) -> String {
    format!(
        "Quiet[Check[With[{{raw = MessageName[Evaluate[{symbol_expr}], \"usage\"]}}, If[StringQ[raw], StringTrim[StringReplace[ToString[raw, OutputForm], {{\"\\t\" -> \" \", \"\\n\" ~~ WhitespaceCharacter... -> \" \", \"\\r\" -> \" \", WhitespaceCharacter.. -> \" \"}}], \"\"], \"\"]], \"\"]]"
    )
}

fn builtin_symbol_names() -> impl Iterator<Item = String> {
    BUILTIN_SYMBOLS.lines().filter_map(|line| {
        let (name, _) = line.split_once('\t')?;
        Some(name.to_string())
    })
}

fn builtin_symbols_for_prefix(prefix: &str) -> Vec<CompletionItem> {
    let short_prefix = short_symbol_name(prefix);
    let mut items: Vec<_> = BUILTIN_SYMBOLS
        .lines()
        .filter_map(|line| {
            let (value, frequency) = line.split_once('\t')?;
            let frequency = frequency.parse().ok();
            fuzzy_matches(value, short_prefix).then_some(CompletionItem {
                value: value.to_string(),
                kind: CompletionKind::Symbol,
                frequency,
                context: Some("System`".to_string()),
            })
        })
        .take(500)
        .collect();

    if prefix.starts_with("System`") {
        for item in &mut items {
            item.value = format!("System`{}", item.value);
        }
    }

    items
}

fn parse_completion_items(lines: Vec<String>) -> Vec<CompletionItem> {
    lines
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let kind = fields.next()?;
            let value = fields.next()?;
            let kind = match kind {
                "symbol" => CompletionKind::Symbol,
                "context" => CompletionKind::Context,
                _ => return None,
            };
            let frequency = fields.next().and_then(|frequency| frequency.parse().ok());
            let context = fields.next().filter(|field| !field.is_empty());
            Some(CompletionItem {
                value: value.to_string(),
                kind,
                frequency,
                context: context.map(str::to_string),
            })
        })
        .collect()
}

fn parse_symbol_details_batch(lines: Vec<String>) -> HashMap<String, CompletionItemDetails> {
    lines
        .into_iter()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.to_string();
            let context = fields.next().filter(|field| !field.is_empty());
            let usage = fields.next().filter(|field| !field.is_empty());
            Some((
                name,
                CompletionItemDetails {
                    context: context.map(str::to_string),
                    usage: usage.map(str::to_string),
                },
            ))
        })
        .collect()
}

fn parse_argument_completion_items(output: &str) -> Vec<ArgumentCompletionItem> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }

            let (kind, value) = line.split_once('\t').unwrap_or(("value", line));
            let kind = match kind {
                "color" => ArgumentCompletionKind::Color,
                "file" => ArgumentCompletionKind::File,
                "directory" => ArgumentCompletionKind::Directory,
                _ => ArgumentCompletionKind::Value,
            };

            Some(ArgumentCompletionItem {
                value: value.to_string(),
                kind,
            })
        })
        .collect()
}

fn wolfram_string_literal(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

struct WolframCompleter {
    source: CompletionSource,
    theme: ThemeHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionItemDetails {
    context: Option<String>,
    usage: Option<String>,
}

impl WolframCompleter {
    fn new(source: CompletionSource, theme: ThemeHandle) -> Self {
        Self { source, theme }
    }
}

impl Completer for WolframCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let styles = self.theme.current().styles();
        if let Some(suggestions) = command_completion_suggestions(line, pos, styles) {
            return suggestions;
        }

        let start = symbol_start(line, pos);
        let prefix = &line[start..pos];
        let short_prefix = short_symbol_name(prefix);
        let option_head = option_context(line, start);
        let argument_context = argument_context(line, start);

        if short_prefix.is_empty() && !prefix.ends_with('`') {
            return Vec::new();
        }

        let mut suggestions = Vec::new();

        let symbols = self.source.symbols_for_prefix(prefix);
        suggestions.extend(symbol_suggestions(
            &symbols,
            prefix,
            start,
            pos,
            &self.source,
            styles,
        ));

        if let Some(head) = option_head {
            let options = self.source.options_for(&head);
            suggestions.extend(option_suggestions(
                &options,
                short_prefix,
                start,
                pos,
                &head,
                styles,
            ));
        }

        if let Some((head, argument_index)) = argument_context {
            let completions = self.source.arguments_for(&head, argument_index);
            suggestions.extend(argument_suggestions(
                &completions,
                short_prefix,
                start,
                pos,
                &head,
                styles,
            ));
        }

        suggestions.sort_by(|left, right| {
            completion_sort_key(left, short_prefix)
                .cmp(&completion_sort_key(right, short_prefix))
                .then_with(|| left.value.cmp(&right.value))
        });
        suggestions.dedup_by(|left, right| left.value == right.value);
        for suggestion in &mut suggestions {
            suggestion.extra = None;
        }
        suggestions
    }
}

fn command_completion_suggestions(
    line: &str,
    pos: usize,
    styles: ThemeStyles,
) -> Option<Vec<Suggestion>> {
    if !line.starts_with(':') || pos > line.len() {
        return None;
    }

    let before_cursor = &line[..pos];
    let command_line = &before_cursor[1..];
    let command_start = command_line
        .find(|ch: char| !ch.is_whitespace())
        .map_or(pos, |idx| idx + 1);
    let command_and_args = command_line.trim_start();

    if command_and_args.is_empty() || !command_and_args.contains(char::is_whitespace) {
        let prefix = command_and_args;
        return Some(command_name_suggestions(prefix, command_start, pos, styles));
    }

    let mut parts = command_and_args.split_whitespace();
    let command = parts.next().unwrap_or_default().to_lowercase();
    let argument_start = before_cursor
        .rfind(char::is_whitespace)
        .map_or(pos, |idx| idx + 1);
    let argument_prefix = &before_cursor[argument_start..pos];
    let has_trailing_space = before_cursor
        .chars()
        .last()
        .is_some_and(char::is_whitespace);

    match command.as_str() {
        "theme" if parts.next().is_none() || !has_trailing_space => Some(theme_arg_suggestions(
            argument_prefix,
            argument_start,
            pos,
            styles,
        )),
        _ => Some(Vec::new()),
    }
}

fn command_name_suggestions(
    prefix: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    [
        ("help", "Show REPL commands"),
        ("theme", "Change syntax highlighting theme"),
        ("quit", "Quit the REPL"),
    ]
    .into_iter()
    .filter(|(value, _)| command_candidate_matches(value, prefix))
    .map(|(value, description)| command_suggestion(value, description, start, end, styles))
    .collect()
}

fn theme_arg_suggestions(
    prefix: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    if prefix.is_empty() {
        return [
            ("dark", theme_description(Theme::Dark)),
            ("light", theme_description(Theme::Light)),
            ("solarized", theme_description(Theme::Solarized)),
            ("gruvbox", theme_description(Theme::Gruvbox)),
            ("monokai", theme_description(Theme::Monokai)),
            ("plain", theme_description(Theme::Plain)),
            ("list", "Browse available themes".to_string()),
            ("show", "Show the current theme".to_string()),
        ]
        .into_iter()
        .map(|(value, description)| command_suggestion(value, &description, start, end, styles))
        .collect();
    }

    let theme_suggestions = Theme::ALL.map(|theme| {
        let description = theme_description(theme);
        (theme.name(), description)
    });
    theme_suggestions
        .into_iter()
        .chain([
            ("solarized-dark", theme_description(Theme::Solarized)),
            ("gruvbox-dark", theme_description(Theme::Gruvbox)),
            ("none", theme_description(Theme::Plain)),
            ("no-color", theme_description(Theme::Plain)),
            ("nocolor", theme_description(Theme::Plain)),
            ("list", "Browse available themes".to_string()),
            ("ls", "Browse available themes".to_string()),
            ("browse", "Browse available themes".to_string()),
            ("show", "Show the current theme".to_string()),
            ("current", "Show the current theme".to_string()),
        ])
        .filter(|(value, _)| command_candidate_matches(value, prefix))
        .map(|(value, description)| command_suggestion(value, &description, start, end, styles))
        .collect()
}

fn theme_description(theme: Theme) -> String {
    if theme == Theme::Plain {
        "Disable syntax highlighting colors".to_string()
    } else {
        format!("Use the {} syntax highlighting theme", theme.name())
    }
}

fn command_candidate_matches(candidate: &str, prefix: &str) -> bool {
    candidate
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
}

fn command_suggestion(
    value: &str,
    description: &str,
    start: usize,
    end: usize,
    styles: ThemeStyles,
) -> Suggestion {
    Suggestion {
        value: value.to_string(),
        description: Some(description.to_string()),
        style: Some(styles.completion_command),
        extra: None,
        span: Span { start, end },
        append_whitespace: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CompletionSourceKind {
    User,
    Builtin,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletionSortMetadata {
    source: CompletionSourceKind,
    frequency: Option<usize>,
}

impl CompletionSortMetadata {
    fn serialize(self) -> String {
        let source = match self.source {
            CompletionSourceKind::User => "user",
            CompletionSourceKind::Builtin => "builtin",
            CompletionSourceKind::Other => "other",
        };
        let frequency = self
            .frequency
            .map(|value| value.to_string())
            .unwrap_or_default();
        format!("source={source};frequency={frequency}")
    }

    fn parse(value: &str) -> Self {
        let mut source = CompletionSourceKind::Other;
        let mut frequency = None;

        for part in value.split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key {
                "source" => {
                    source = match value {
                        "user" => CompletionSourceKind::User,
                        "builtin" => CompletionSourceKind::Builtin,
                        _ => CompletionSourceKind::Other,
                    }
                }
                "frequency" => frequency = value.parse().ok(),
                _ => {}
            }
        }

        Self { source, frequency }
    }
}

fn completion_sort_key(suggestion: &Suggestion, short_prefix: &str) -> (usize, Option<usize>) {
    let metadata = suggestion
        .extra
        .as_ref()
        .and_then(|extra| extra.first())
        .map(|value| CompletionSortMetadata::parse(value))
        .unwrap_or(CompletionSortMetadata {
            source: CompletionSourceKind::Other,
            frequency: None,
        });
    let source_priority = match metadata.source {
        CompletionSourceKind::User => 0,
        CompletionSourceKind::Other => 1,
        CompletionSourceKind::Builtin => 2,
    };
    let score =
        completion_score(&suggestion.value, short_prefix, metadata.frequency).unwrap_or(usize::MAX);
    (source_priority, Some(score))
}

/// How many not-yet-known symbols get an eager background usage lookup per
/// `complete()` call. Final on-screen order isn't known yet at this point (the
/// caller sorts afterwards), so this is a rank-order approximation rather than
/// an exact "visible rows" count; it exists to bound a single background
/// kernel round trip so a broad prefix (matching hundreds of names) can't
/// balloon it. Symbols beyond this cutoff simply pick up their usage text
/// once they've been requested (and cached) on an earlier call.
const USAGE_LOOKAHEAD: usize = 20;

fn symbol_suggestions(
    symbols: &[CompletionItem],
    prefix: &str,
    start: usize,
    end: usize,
    source: &CompletionSource,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    let context_prefix = prefix.rfind('`').map(|idx| &prefix[..=idx]);
    let matches: Vec<(&CompletionItem, String)> = symbols
        .iter()
        .filter_map(|candidate| {
            let value =
                if candidate.kind == CompletionKind::Symbol && !candidate.value.contains('`') {
                    context_prefix
                        .map(|context| format!("{context}{}", candidate.value))
                        .unwrap_or_else(|| candidate.value.clone())
                } else {
                    candidate.value.clone()
                };
            let match_pattern = if candidate.kind == CompletionKind::Context {
                prefix
            } else {
                short_symbol_name(prefix)
            };
            let match_value = if candidate.kind == CompletionKind::Context {
                value.as_str()
            } else {
                short_symbol_name(&value)
            };

            fuzzy_matches(match_value, match_pattern).then_some((candidate, value))
        })
        .collect();

    let wanted: Vec<String> = matches
        .iter()
        .filter(|(candidate, _)| candidate.kind == CompletionKind::Symbol)
        .take(USAGE_LOOKAHEAD)
        .map(|(_, value)| value.clone())
        .collect();
    let usage = source.usage_details(&wanted);

    matches
        .into_iter()
        .map(|(candidate, value)| {
            let details = match candidate.kind {
                CompletionKind::Symbol => {
                    usage.get(&value).cloned().unwrap_or(CompletionItemDetails {
                        context: candidate.context.clone(),
                        usage: None,
                    })
                }
                CompletionKind::Context => CompletionItemDetails {
                    context: candidate.context.clone(),
                    usage: None,
                },
            };

            let (description, style) = match candidate.kind {
                CompletionKind::Symbol => (
                    symbol_completion_description(&details),
                    styles.completion_symbol,
                ),
                CompletionKind::Context => (
                    context_completion_description(&details),
                    styles.completion_context,
                ),
            };

            Suggestion {
                value,
                description: Some(description),
                style: Some(style),
                extra: Some(vec![
                    CompletionSortMetadata {
                        source: completion_source_kind(candidate),
                        frequency: candidate.frequency,
                    }
                    .serialize(),
                ]),
                span: Span { start, end },
                append_whitespace: false,
            }
        })
        .collect()
}

fn completion_source_kind(candidate: &CompletionItem) -> CompletionSourceKind {
    match candidate.kind {
        CompletionKind::Context => CompletionSourceKind::User,
        CompletionKind::Symbol if candidate.context.as_deref() == Some("System`") => {
            CompletionSourceKind::Builtin
        }
        CompletionKind::Symbol => CompletionSourceKind::User,
    }
}

fn symbol_completion_description(details: &CompletionItemDetails) -> String {
    let mut parts = vec!["symbol".to_string()];

    if let Some(context) = &details.context {
        parts.push(format!("Context: {context}"));
    }

    if let Some(usage) = &details.usage {
        parts.push(format!("Usage: {usage}"));
    }

    parts.join("\n")
}

fn context_completion_description(details: &CompletionItemDetails) -> String {
    details
        .context
        .as_ref()
        .map(|context| format!("context\nContext: {context}"))
        .unwrap_or_else(|| "context".to_string())
}

fn option_suggestions(
    options: &[String],
    prefix: &str,
    start: usize,
    end: usize,
    head: &str,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    options
        .iter()
        .filter(|candidate| fuzzy_matches(candidate, prefix))
        .map(|candidate| Suggestion {
            value: candidate.clone(),
            description: Some(format!("option for {head}")),
            style: Some(styles.completion_option),
            extra: None,
            span: Span { start, end },
            append_whitespace: false,
        })
        .collect()
}

fn argument_suggestions(
    completions: &[ArgumentCompletionItem],
    prefix: &str,
    start: usize,
    end: usize,
    head: &str,
    styles: ThemeStyles,
) -> Vec<Suggestion> {
    completions
        .iter()
        .filter(|candidate| fuzzy_matches(&candidate.value, prefix))
        .map(|candidate| Suggestion {
            value: candidate.value.clone(),
            description: Some(argument_completion_description(candidate.kind, head)),
            style: Some(argument_completion_style(candidate.kind, styles)),
            extra: None,
            span: Span { start, end },
            append_whitespace: false,
        })
        .collect()
}

fn argument_completion_description(kind: ArgumentCompletionKind, head: &str) -> String {
    match kind {
        ArgumentCompletionKind::Value => format!("argument for {head}"),
        ArgumentCompletionKind::Color => "color".to_string(),
        ArgumentCompletionKind::File => "file path".to_string(),
        ArgumentCompletionKind::Directory => "directory path".to_string(),
    }
}

fn argument_completion_style(kind: ArgumentCompletionKind, styles: ThemeStyles) -> Style {
    match kind {
        ArgumentCompletionKind::Value => styles.completion_argument,
        ArgumentCompletionKind::Color => styles.completion_color,
        ArgumentCompletionKind::File => styles.completion_file,
        ArgumentCompletionKind::Directory => styles.completion_directory,
    }
}

fn fuzzy_matches(candidate: &str, pattern: &str) -> bool {
    completion_score(candidate, pattern, None).is_some()
}

fn completion_score(candidate: &str, pattern: &str, frequency: Option<usize>) -> Option<usize> {
    let candidate_lower = candidate.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let frequency_bonus = frequency.map(|value| value.min(99)).unwrap_or(0);
    let weigh = |score: usize| score.saturating_sub(frequency_bonus);

    if pattern_lower.is_empty() {
        return Some(weigh(100));
    }

    if candidate_lower.starts_with(&pattern_lower) {
        return Some(weigh(100));
    }

    if acronym_matches(candidate, pattern) {
        return Some(weigh(200 + candidate.chars().count()));
    }

    if prefix_plus_word_initials_matches(candidate, pattern) {
        return Some(weigh(250 + candidate.chars().count()));
    }

    if pattern.chars().count() < 3 {
        return None;
    }

    fuzzy_subsequence_score(candidate, pattern).map(|score| weigh(300 + score))
}

fn fuzzy_subsequence_score(candidate: &str, pattern: &str) -> Option<usize> {
    let candidate: Vec<char> = candidate.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut last_match: Option<usize> = None;
    let mut search_from = 0;
    let mut skipped = 0;

    for wanted in &pattern {
        let found = candidate
            .iter()
            .enumerate()
            .skip(search_from)
            .find_map(|(idx, ch)| ch.eq_ignore_ascii_case(wanted).then_some(idx))?;

        if let Some(last) = last_match {
            skipped += found.saturating_sub(last + 1);
        }

        last_match = Some(found);
        search_from = found + 1;
    }

    let end = last_match?;
    skipped += candidate.len().saturating_sub(end + 1);

    if skipped > pattern.len() {
        return None;
    };

    Some(skipped)
}

fn acronym_matches(candidate: &str, pattern: &str) -> bool {
    let acronym: String = candidate
        .chars()
        .filter(|ch| ch.is_uppercase())
        .flat_map(char::to_lowercase)
        .collect();

    !acronym.is_empty() && acronym == pattern.to_lowercase()
}

fn prefix_plus_word_initials_matches(candidate: &str, pattern: &str) -> bool {
    let mut candidate_chars = candidate.chars().peekable();
    let mut pattern_chars = pattern.chars().peekable();

    while let (Some(candidate_char), Some(pattern_char)) =
        (candidate_chars.peek(), pattern_chars.peek())
    {
        if !candidate_char.eq_ignore_ascii_case(pattern_char) {
            break;
        }
        candidate_chars.next();
        pattern_chars.next();
    }

    if pattern_chars.peek().is_none() {
        return true;
    }

    let initials: String = candidate_chars
        .filter(|ch| ch.is_uppercase())
        .flat_map(char::to_lowercase)
        .collect();
    let remaining_pattern: String = pattern_chars.flat_map(char::to_lowercase).collect();

    !remaining_pattern.is_empty() && initials.starts_with(&remaining_pattern)
}

struct WolframHighlighter {
    symbols: HashSet<String>,
    theme: ThemeHandle,
}

impl WolframHighlighter {
    fn new(symbols: HashSet<String>, theme: ThemeHandle) -> Self {
        Self { symbols, theme }
    }
}

impl Highlighter for WolframHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        highlight_wolfram_text(line, self.theme.current().styles(), Some(&self.symbols))
    }
}

fn highlight_wolfram_text(
    line: &str,
    styles: ThemeStyles,
    symbols: Option<&HashSet<String>>,
) -> StyledText {
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
            let style = if symbols.is_none_or(|symbols| symbols.contains(short_symbol_name(word))) {
                styles.builtin_symbol
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

fn symbol_start(line: &str, pos: usize) -> usize {
    line[..pos]
        .rfind(|c: char| !is_symbol_char(c))
        .map_or(0, |idx| idx + 1)
}

fn short_symbol_name(symbol: &str) -> &str {
    symbol.rsplit('`').next().unwrap_or(symbol)
}

fn is_symbol_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '$' || ch == '`'
}

fn is_symbol_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '$' || ch == '`'
}

fn is_qualified_symbol_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(is_symbol_char)
}

fn option_context(line: &str, pos: usize) -> Option<String> {
    let bracket = innermost_open_square_bracket(line, pos)?;
    let args = &line[bracket + 1..pos];
    if !has_top_level_comma(args) {
        return None;
    }
    symbol_before(line, bracket)
}

fn argument_context(line: &str, pos: usize) -> Option<(String, usize)> {
    let bracket = innermost_open_square_bracket(line, pos)?;
    let head = symbol_before(line, bracket)?;
    let argument_index = top_level_comma_count(&line[bracket + 1..pos]);
    Some((head, argument_index))
}

fn innermost_open_square_bracket(line: &str, pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (idx, ch) in line[..pos].char_indices().rev() {
        match ch {
            ']' | '}' | ')' => depth += 1,
            '[' => {
                if depth == 0 {
                    return Some(idx);
                }
                depth -= 1;
            }
            '{' | '(' if depth > 0 => depth -= 1,
            _ => {}
        }
    }
    None
}

fn symbol_before(line: &str, end: usize) -> Option<String> {
    let before = line[..end].trim_end();
    let end = before.len();
    let start = before[..end]
        .rfind(|c: char| !is_symbol_char(c))
        .map_or(0, |idx| idx + 1);
    let symbol = &before[start..end];
    (!symbol.is_empty()).then(|| symbol.to_string())
}

fn has_top_level_comma(input: &str) -> bool {
    top_level_comma_count(input) > 0
}

fn top_level_comma_count(input: &str) -> usize {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut count = 0usize;

    for ch in input.chars() {
        if in_string {
            if ch == '"' && !escaped {
                in_string = false;
            }
            escaped = ch == '\\' && !escaped;
            if ch != '\\' {
                escaped = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' if depth > 0 => depth -= 1,
            ',' if depth == 0 => count += 1,
            _ => {}
        }
    }

    count
}

#[cfg(feature = "wstp")]
mod native_wstp {
    use super::*;
    use anyhow::anyhow;
    use std::{process::Child, thread, time::Duration};
    use wolfram_expr::{Expr, Symbol};
    use wstp::{Link, Protocol, sys};

    pub(crate) struct WstpKernelClient {
        process: Child,
        link: Option<Link>,
    }

    impl WstpKernelClient {
        pub(crate) fn launch() -> Result<Self> {
            let path = kernel_path()?;
            let mut link = Link::listen(Protocol::SharedMemory, "")
                .map_err(|err| anyhow!("failed to create WSTP listener: {err:?}"))?;
            let link_name = link.link_name();
            let process = Command::new(path)
                .arg("-wstp")
                .arg("-linkprotocol")
                .arg("SharedMemory")
                .arg("-linkconnect")
                .arg("-linkname")
                .arg(&link_name)
                .spawn()
                .context("failed to launch WolframKernel in WSTP mode")?;

            link.activate()
                .map_err(|err| anyhow!("failed to activate WSTP link: {err:?}"))?;

            Ok(Self {
                process,
                link: Some(link),
            })
        }

        pub(crate) fn evaluate_once(
            &mut self,
            input: &str,
            line_number: Option<(usize, &ThemeHandle)>,
        ) -> Result<()> {
            let result = self.evaluate_expr(input)?;
            let text = self.expr_to_output_string(result)?;
            if !text.is_empty() {
                if let Some((line_number, theme)) = line_number {
                    print!("Out[{line_number}]= ");
                    print_highlighted(&text, theme.current());
                } else {
                    println!("{text}");
                }
            }
            Ok(())
        }

        pub(crate) fn evaluate_to_string(&mut self, input: &str) -> Result<String> {
            let expr = call(
                "System`ToString",
                vec![
                    call("System`ToExpression", vec![Expr::string(input)]),
                    symbol("System`InputForm"),
                ],
            );
            self.evaluate_packet_to_string(&expr)
        }

        fn evaluate_expr(&mut self, input: &str) -> Result<Expr> {
            let wrapped_input = format!(
                "Module[{{promptedInputString, promptedInput}}, SetAttributes[{{promptedInputString, promptedInput}}, HoldAll]; promptedInputString[prompt_] := (WriteString[$Output, ToString[Unevaluated[prompt], OutputForm]]; InputString[]); promptedInput[prompt_] := ToExpression[promptedInputString[prompt]]; ReleaseHold[ToExpression[{}, InputForm, HoldComplete] /. {{HoldPattern[InputString[prompt_]] :> promptedInputString[prompt], HoldPattern[Input[prompt_]] :> promptedInput[prompt]}}]]",
                wolfram_string_literal(input)
            );
            let expr = call("System`ToExpression", vec![Expr::string(&wrapped_input)]);
            let link = self.link.as_mut().context("WSTP link is closed")?;
            link.put_eval_packet(&expr)
                .map_err(|err| anyhow!("failed to send WSTP evaluate packet: {err:?}"))?;
            link.flush()
                .map_err(|err| anyhow!("failed to flush WSTP link: {err:?}"))?;

            loop {
                let packet = match link.raw_next_packet() {
                    Ok(packet) => packet,
                    Err(err) => {
                        if let Some(code) =
                            Self::child_exit_code_after_link_error(&mut self.process)
                        {
                            return Err(KernelExit::new(code).into());
                        }
                        return Err(anyhow!("failed to read WSTP packet: {err:?}"));
                    }
                };
                match packet {
                    sys::RETURNPKT => {
                        let expr = link
                            .get_expr_with_resolver(&mut |name| {
                                (!name.contains('`'))
                                    .then(|| Symbol::new(&format!("System`{name}")))
                            })
                            .map_err(|err| anyhow!("failed to read WSTP return value: {err:?}"))?;
                        return Ok(expr);
                    }
                    sys::TEXTPKT | sys::RETURNTEXTPKT => {
                        let text = link
                            .get_string()
                            .map_err(|err| anyhow!("failed to read WSTP text packet: {err:?}"))?;
                        print!("{text}");
                        io::stdout().flush().context("failed to flush stdout")?;
                        link.new_packet()
                            .map_err(|err| anyhow!("failed to finish WSTP text packet: {err:?}"))?;
                    }
                    sys::CALLPKT => {
                        Self::handle_call_packet(link)?;
                    }
                    sys::INPUTNAMEPKT => {
                        link.new_packet().map_err(|err| {
                            anyhow!("failed to finish WSTP input name packet: {err:?}")
                        })?;
                    }
                    sys::INPUTPKT | sys::INPUTSTRPKT => {
                        let mut input = String::new();
                        io::stdin()
                            .read_line(&mut input)
                            .context("failed to read stdin for Wolfram input request")?;
                        let input = input.trim_end_matches(['\r', '\n']);
                        link.new_packet().map_err(|err| {
                            anyhow!("failed to finish WSTP input request packet: {err:?}")
                        })?;
                        link.put_str(input).map_err(|err| {
                            anyhow!("failed to send WSTP input response: {err:?}")
                        })?;
                        link.end_packet().map_err(|err| {
                            anyhow!("failed to finish WSTP input response: {err:?}")
                        })?;
                        link.flush().map_err(|err| {
                            anyhow!("failed to flush WSTP input response: {err:?}")
                        })?;
                    }
                    _ => {
                        link.new_packet()
                            .map_err(|err| anyhow!("failed to skip WSTP packet: {err:?}"))?;
                    }
                }
            }
        }

        fn handle_call_packet(link: &mut Link) -> Result<()> {
            let call_id = link
                .get_expr()
                .map_err(|err| anyhow!("failed to read WSTP call packet: {err:?}"))?;
            link.new_packet()
                .map_err(|err| anyhow!("failed to finish WSTP call packet: {err:?}"))?;
            bail!("unsupported WSTP call packet: {call_id:?}")
        }

        fn expr_to_output_string(&mut self, expr: Expr) -> Result<String> {
            let expr = call("System`ToString", vec![expr, symbol("System`InputForm")]);
            self.evaluate_packet_to_string(&expr)
        }

        fn evaluate_packet_to_string(&mut self, expr: &Expr) -> Result<String> {
            let link = self.link.as_mut().context("WSTP link is closed")?;
            link.put_eval_packet(expr)
                .map_err(|err| anyhow!("failed to send WSTP evaluate packet: {err:?}"))?;
            link.flush()
                .map_err(|err| anyhow!("failed to flush WSTP link: {err:?}"))?;

            loop {
                let packet = match link.raw_next_packet() {
                    Ok(packet) => packet,
                    Err(err) => {
                        if let Some(code) =
                            Self::child_exit_code_after_link_error(&mut self.process)
                        {
                            return Err(KernelExit::new(code).into());
                        }
                        return Err(anyhow!("failed to read WSTP packet: {err:?}"));
                    }
                };
                match packet {
                    sys::RETURNPKT => {
                        let text = link
                            .get_string()
                            .map_err(|err| anyhow!("failed to read WSTP return value: {err:?}"))?;
                        return Ok(text);
                    }
                    sys::TEXTPKT | sys::RETURNTEXTPKT => {
                        let text = link
                            .get_string()
                            .map_err(|err| anyhow!("failed to read WSTP text packet: {err:?}"))?;
                        print!("{text}");
                        io::stdout().flush().context("failed to flush stdout")?;
                        link.new_packet()
                            .map_err(|err| anyhow!("failed to finish WSTP text packet: {err:?}"))?;
                    }
                    _ => {
                        link.new_packet()
                            .map_err(|err| anyhow!("failed to skip WSTP packet: {err:?}"))?;
                    }
                }
            }
        }

        fn child_exit_code_after_link_error(process: &mut Child) -> Option<i32> {
            for _ in 0..20 {
                match process.try_wait() {
                    Ok(Some(status)) => return status.code(),
                    Ok(None) => thread::sleep(Duration::from_millis(50)),
                    Err(_) => return None,
                }
            }
            None
        }

        fn stop_child(&mut self) {
            if let Some(link) = self.link.take() {
                std::mem::forget(link);
            }

            for _ in 0..20 {
                if self.process.try_wait().ok().flatten().is_some() {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    impl Drop for WstpKernelClient {
        fn drop(&mut self) {
            self.stop_child();
        }
    }

    fn symbol(name: &str) -> Expr {
        Expr::symbol(Symbol::try_new(name).expect("internal symbol names are qualified"))
    }

    fn call(head: &str, args: Vec<Expr>) -> Expr {
        Expr::normal(symbol(head), args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn test_styles() -> ThemeStyles {
        Theme::Dark.styles()
    }

    /// A `KernelBackend` that never touches a real kernel, for testing
    /// completion logic without a Wolfram installation. `delay` lets a test
    /// simulate a slow/blocking kernel to prove callers don't wait on it.
    struct FakeBackend {
        symbols: HashMap<String, Vec<CompletionItem>>,
        details: HashMap<String, CompletionItemDetails>,
        delay: Duration,
    }

    impl FakeBackend {
        fn empty() -> Self {
            Self {
                symbols: HashMap::new(),
                details: HashMap::new(),
                delay: Duration::ZERO,
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
            thread::sleep(self.delay);
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

        fn load_argument_completions(
            &self,
            _head: &str,
            _argument_index: usize,
        ) -> Result<Vec<ArgumentCompletionItem>> {
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
    fn detects_frontend_argument_context() {
        let line = "Plot[x, {x, 0, 1}, Pl";
        let token_start = symbol_start(line, line.len());
        assert_eq!(
            argument_context(line, token_start),
            Some(("Plot".to_string(), 2))
        );
    }

    #[test]
    fn ignores_nested_commas_for_argument_index() {
        let line = "Plot[{x, x^2}, {x, 0, 1}, Pl";
        let token_start = symbol_start(line, line.len());
        assert_eq!(
            argument_context(line, token_start),
            Some(("Plot".to_string(), 2))
        );
    }

    #[test]
    fn escapes_wolfram_string_literals() {
        assert_eq!(wolfram_string_literal("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn strips_subprocess_evaluation_completion_marker() {
        assert_eq!(
            strip_evaluation_complete_marker("2\n__done__\n", "\n__done__\n"),
            Some("2")
        );
        assert_eq!(
            strip_evaluation_complete_marker("2\n", "\n__done__\n"),
            None
        );
    }

    #[test]
    fn builds_frontend_argument_completion_query() {
        let query = frontend_argument_completion_query(Path::new("/opt/Wolfram"), "Plot", 2);
        assert!(query.contains("/opt/Wolfram"));
        assert!(query.contains("\"Plot\""));
        assert!(query.contains("index = 2"));
        assert!(query.contains("AddSpecialArgCompletion"));
    }

    #[test]
    fn symbol_completion_query_loads_candidates_for_fuzzy_matching() {
        let query = symbol_completion_query("LP");
        assert!(query.contains("base = If[StringEndsQ[p, \"`\"], p, p]"));
        assert!(query.contains("Names[base <> \"*\"]"));
        assert!(query.contains("Contexts[]"));
        assert!(query.contains("ToString[Context @@ sym, OutputForm]"));
        assert!(!query.contains("Context @@ sym, InputForm"));
        assert!(!query.contains("WolframLanguageData"));
        // Usage messages are the expensive part of a completion query (many
        // symbols can match a short prefix); this query must stay name+context
        // only so it stays fast, and fetch usage separately in small batches.
        assert!(!query.contains("MessageName"));
    }

    #[test]
    fn symbol_details_batch_query_loads_context_and_usage_for_explicit_symbols() {
        let query = symbol_details_batch_query(&["Plot".to_string(), "Sin".to_string()]);
        assert!(query.contains("\"Plot\""));
        assert!(query.contains("\"Sin\""));
        assert!(query.contains("MessageName[Evaluate[ReleaseHold[sym]], \"usage\"]"));
        assert!(query.contains("If[StringQ[raw]"));
        assert!(query.contains("ToString[Context @@ sym, OutputForm]"));
        assert!(!query.contains("Context @@ sym, InputForm"));
        assert!(!query.contains("Unevaluated[MessageName"));
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
    fn parses_repl_commands() {
        assert_eq!(parse_repl_command(":help").unwrap(), ReplCommand::Help);
        assert_eq!(parse_repl_command(":?").unwrap(), ReplCommand::Help);
        assert_eq!(
            parse_repl_command(":theme").unwrap(),
            ReplCommand::Theme(ThemeCommand::Cycle)
        );
        assert_eq!(
            parse_repl_command(":theme show").unwrap(),
            ReplCommand::Theme(ThemeCommand::Show)
        );
        assert_eq!(
            parse_repl_command(":theme light").unwrap(),
            ReplCommand::Theme(ThemeCommand::Set(Theme::Light))
        );
        assert_eq!(
            parse_repl_command(":theme DARK").unwrap(),
            ReplCommand::Theme(ThemeCommand::Set(Theme::Dark))
        );
        assert_eq!(
            parse_repl_command(":theme no-color").unwrap(),
            ReplCommand::Theme(ThemeCommand::Set(Theme::Plain))
        );
        assert_eq!(parse_repl_command(":q").unwrap(), ReplCommand::Quit);
    }

    #[test]
    fn rejects_unknown_or_malformed_repl_commands() {
        assert!(parse_repl_command(":unknown").is_err());
        assert!(parse_repl_command(":theme neon").is_err());
        assert!(parse_repl_command(":quit now").is_err());
    }

    #[test]
    fn completes_repl_command_names_only_at_line_start() {
        let suggestions = command_completion_suggestions(":t", 2, test_styles()).unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "theme");
        assert_eq!(suggestions[0].span.start, 1);
        assert_eq!(suggestions[0].span.end, 2);

        assert!(command_completion_suggestions("x:t", 3, test_styles()).is_none());
    }

    #[test]
    fn completes_theme_command_arguments() {
        let suggestions = command_completion_suggestions(":theme l", 8, test_styles()).unwrap();
        let matching_values: Vec<_> = suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect();
        assert_eq!(matching_values, vec!["light", "list", "ls"]);
        assert_eq!(suggestions[0].span.start, 7);
        assert_eq!(suggestions[0].span.end, 8);

        let values: Vec<_> = command_completion_suggestions(":theme ", 7, test_styles())
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
        );
        let mut completer = WolframCompleter::new(source, ThemeHandle::new(Theme::Dark));

        assert!(completer.complete(":P", 2).is_empty());
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
    fn parses_typed_argument_completion_items() {
        assert_eq!(
            parse_argument_completion_items("color\tRed\nfile\t./\ndirectory\t./\nplain"),
            vec![
                ArgumentCompletionItem {
                    value: "Red".to_string(),
                    kind: ArgumentCompletionKind::Color,
                },
                ArgumentCompletionItem {
                    value: "./".to_string(),
                    kind: ArgumentCompletionKind::File,
                },
                ArgumentCompletionItem {
                    value: "./".to_string(),
                    kind: ArgumentCompletionKind::Directory,
                },
                ArgumentCompletionItem {
                    value: "plain".to_string(),
                    kind: ArgumentCompletionKind::Value,
                },
            ]
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
        };
        let source = CompletionSource::with_backend(Arc::new(backend), Arc::new(AtomicU64::new(0)));
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

        let suggestions =
            symbol_suggestions(&[candidate], "System`P", 0, 8, &source, test_styles());

        assert_eq!(suggestions[0].value, "System`Plot");
        assert_eq!(
            suggestions[0].description.as_deref(),
            Some("symbol\nContext: System`\nUsage: Plot[f, {x, xmin, xmax}] plots f.")
        );
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
    fn ranks_direct_matches_before_longer_fuzzy_symbol_matches() {
        let source = CompletionSource::with_backend(
            Arc::new(FakeBackend::empty()),
            Arc::new(AtomicU64::new(0)),
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

        let argument_suggestions = argument_suggestions(
            &[ArgumentCompletionItem {
                value: "PlotRange".to_string(),
                kind: ArgumentCompletionKind::Value,
            }],
            "PlR",
            0,
            3,
            "Plot",
            test_styles(),
        );
        assert_eq!(argument_suggestions[0].value, "PlotRange");
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
        };
        let source = CompletionSource::with_backend(Arc::new(backend), Arc::new(AtomicU64::new(0)));
        let mut completer = WolframCompleter::new(source, ThemeHandle::new(Theme::Dark));

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
}
