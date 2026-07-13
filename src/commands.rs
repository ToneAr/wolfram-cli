use std::{
    env, fs,
    io::{self, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::theme::{
    CONFIG_SCHEMA_URL, CommandConfig, Theme, ThemeHandle, ThemeRegistry, UserConfig, config_file,
    load_user_config, print_theme_browser, save_user_config,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAction {
    Continue,
    OpenHistory,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigMode {
    User,
    Ephemeral,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplCommand {
    Clear,
    Config(ConfigCommand),
    Help,
    Settings,
    History,
    Theme(ThemeCommand),
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigCommand {
    Show,
    Edit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThemeCommand {
    Cycle,
    List,
    Show,
    Set(Theme),
}

pub(crate) fn execute_repl_command(
    input: &str,
    theme: &ThemeHandle,
    use_color: bool,
    config_mode: ConfigMode,
) -> CommandAction {
    let command = match parse_repl_command(input, theme.registry()) {
        Ok(command) => command,
        Err(message) => {
            println!("{message}");
            return CommandAction::Continue;
        }
    };

    match command {
        ReplCommand::Clear => {
            print!("\x1B[2J\x1B[H");
            let _ = io::stdout().flush();
            CommandAction::Continue
        }
        ReplCommand::Config(ConfigCommand::Show) => {
            match config_mode {
                ConfigMode::User => match config_file() {
                    Some(path) => println!("Config: {}", path.display()),
                    None => println!("Could not determine the config file location."),
                },
                ConfigMode::Ephemeral => {
                    println!(
                        "Config: skipped by --skip-config; using in-memory defaults for this session."
                    )
                }
            }
            CommandAction::Continue
        }
        ReplCommand::Config(ConfigCommand::Edit) => {
            match config_mode {
                ConfigMode::User => {
                    if let Err(err) = edit_config_file() {
                        println!("Could not open config: {err:#}");
                    }
                }
                ConfigMode::Ephemeral => println!(
                    "Config editing is disabled by --skip-config for this ephemeral session."
                ),
            }
            CommandAction::Continue
        }
        ReplCommand::Help => {
            print_command_help(theme.current(), theme.registry());
            CommandAction::Continue
        }
        ReplCommand::Settings => {
            match config_mode {
                ConfigMode::User => {
                    if let Err(err) = run_settings_menu(theme, use_color) {
                        println!("Could not update settings: {err:#}");
                    }
                }
                ConfigMode::Ephemeral => println!(
                    "Settings are disabled by --skip-config for this ephemeral session. Use :theme for temporary theme changes."
                ),
            }
            CommandAction::Continue
        }
        ReplCommand::History => CommandAction::OpenHistory,
        ReplCommand::Theme(ThemeCommand::Cycle) => {
            if !use_color {
                println!("Color is disabled by --no-color; theme remains plain.");
                return CommandAction::Continue;
            }
            let next = theme.next();
            set_theme(theme, next);
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::List) => {
            print_theme_browser(theme.current(), theme.registry(), use_color);
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::Show) => {
            println!("Theme: {}", theme.current().name());
            CommandAction::Continue
        }
        ReplCommand::Theme(ThemeCommand::Set(next)) => {
            if !use_color && !next.is_plain() {
                println!("Color is disabled by --no-color; theme remains plain.");
                return CommandAction::Continue;
            }
            set_theme(theme, next);
            CommandAction::Continue
        }
        ReplCommand::Quit => CommandAction::Quit,
    }
}

pub(crate) fn execute_shell_escape(command: &str) {
    if let Err(err) = run_shell_escape(command) {
        println!("Could not run shell command: {err:#}");
    }
}

fn print_command_help(theme: Theme, registry: &ThemeRegistry) {
    println!("Commands:");
    println!("  :clear                Clear the console.");
    println!("  :setting | :settings  Open the friendly settings menu.");
    println!("  :config | :conf       Open the friendly settings menu.");
    println!("  :config show          Show config file location.");
    println!("  :config edit          Open config file in $EDITOR.");
    println!("  :help | :h | :?       Show this help.");
    println!("  :history | :hist      Open the history browser.");
    println!("                        Press a key to search/navigate.");
    println!("                        Can also be opened using CTRL+r");
    println!(
        "  :!{{command}}           Run a command in your shell with inherited stdin/stdout/stderr."
    );
    println!(
        "  :theme                Cycle theme. Current: {}.",
        theme.name()
    );
    println!("  :theme {{name}}         Set syntax highlighting theme. Options:");
    for available_theme in registry.themes() {
        println!("                        | {}", available_theme.name());
    }
    println!("  :theme list           Browse available themes.");
    println!("  :theme show           Show the current theme.");
    println!("  :quit | :q            Quit the REPL.");
}

fn set_theme(theme: &ThemeHandle, next: Theme) {
    let name = next.name().to_string();
    match theme.set(next) {
        Ok(()) => println!("Theme: {name}"),
        Err(err) => println!("Theme: {name} (warning: could not save preference: {err:#})"),
    }
}

fn run_settings_menu(theme: &ThemeHandle, use_color: bool) -> Result<()> {
    let mut config = load_user_config();
    print_settings_menu(&config, theme);
    loop {
        let choice = read_menu_line("Choose a setting to change (or q to leave): ")?;
        match choice.trim().to_ascii_lowercase().as_str() {
            "q" | "quit" | "exit" | "done" => break,
            "1" | "theme" => configure_theme(&mut config, theme, use_color)?,
            "2" | "lightweight" => configure_bool(
                &mut config,
                "lightweight",
                "Disable optional background, completion, and history overhead",
                |command| &mut command.lightweight,
            )?,
            "3" | "no-color" | "color" => configure_bool(
                &mut config,
                "no-color",
                "Disable ANSI coloring on startup",
                |command| &mut command.no_color,
            )?,
            "4" | "no-welcome" | "welcome" => configure_bool(
                &mut config,
                "no-welcome",
                "Hide the welcome banner on startup",
                |command| &mut command.no_welcome,
            )?,
            "5" | "no-prompt" | "prompt" => configure_bool(
                &mut config,
                "no-prompt",
                "Disable REPL prompts and the welcome banner",
                |command| &mut command.no_prompt,
            )?,
            "6" | "completion-ghost-text" | "ghost" => configure_bool(
                &mut config,
                "completion-ghost-text",
                "Enable inline ghost text completion hints",
                |command| &mut command.completion_ghost_text,
            )?,
            "no-completion-ghost-text" => configure_bool(
                &mut config,
                "no-completion-ghost-text",
                "Explicitly disable inline ghost text completion hints",
                |command| &mut command.no_completion_ghost_text,
            )?,
            "7" | "no-completion-menu" | "menu" => configure_bool(
                &mut config,
                "no-completion-menu",
                "Disable the popup completion menu",
                |command| &mut command.no_completion_menu,
            )?,
            "8" | "no-tree-sitter-highlighting" | "tree-sitter" | "semantic" => configure_bool(
                &mut config,
                "no-tree-sitter-highlighting",
                "Disable tree-sitter semantic highlighting overlays",
                |command| &mut command.no_tree_sitter_highlighting,
            )?,
            "9" | "linkconnect" => configure_bool(
                &mut config,
                "linkconnect",
                "Connect to an existing WSTP link on startup",
                |command| &mut command.linkconnect,
            )?,
            "10" | "linkname" => configure_string(
                &mut config,
                "linkname",
                "WSTP link name to use with linkconnect",
                |command| &mut command.linkname,
            )?,
            "11" | "linkprotocol" => configure_link_protocol(&mut config)?,
            "12" | "linkmode" => configure_string(
                &mut config,
                "linkmode",
                "WSTP link mode for launching or connecting",
                |command| &mut command.linkmode,
            )?,
            "13" | "linkoptions" => configure_u32(
                &mut config,
                "linkoptions",
                "WSTP link options integer",
                |command| &mut command.linkoptions,
            )?,
            "14" | "linkinit" => configure_bool(
                &mut config,
                "linkinit",
                "Initialize linkoptions=4 connected kernels in Wolfie's launch directory",
                |command| &mut command.linkinit,
            )?,
            "e" | "edit" => {
                edit_config_file()?;
                config = load_user_config();
            }
            "p" | "path" | "show" => print_config_location(),
            other => println!(
                "I don't know {other:?}. Pick a number, e to edit, p for path, or q to leave."
            ),
        }
    }
    println!("Settings menu closed.");
    Ok(())
}

fn print_settings_menu(config: &UserConfig, theme: &ThemeHandle) {
    let current_theme = theme.current();
    let styles = current_theme.styles();
    let italic = nu_ansi_term::Style::new().italic();
    let value_style = styles.comment;
    let title_style = nu_ansi_term::Style::new().bold();
    let underline = title_style.underline();

    println!();
    println!("{}", underline.paint("Wolfie settings:"));
    println!();
    let linkoptions = config
        .command
        .linkoptions
        .map_or_else(|| "default".to_string(), |value| value.to_string());
    let options = [
        (
            "1.  theme                 ",
            config.theme.as_deref().unwrap_or(current_theme.name()),
        ),
        (
            "2.  lightweight           ",
            option_label(config.command.lightweight),
        ),
        (
            "3.  no-color              ",
            option_label(config.command.no_color),
        ),
        (
            "4.  no-welcome            ",
            option_label(config.command.no_welcome),
        ),
        (
            "5.  no-prompt             ",
            option_label(config.command.no_prompt),
        ),
        (
            "6.  completion-ghost-text ",
            option_label(config.command.completion_ghost_text),
        ),
        (
            "7.  no-completion-menu    ",
            option_label(config.command.no_completion_menu),
        ),
        (
            "8.  no-tree-sitter        ",
            option_label(config.command.no_tree_sitter_highlighting),
        ),
        (
            "9.  linkconnect           ",
            option_label(config.command.linkconnect),
        ),
        (
            "10. linkname              ",
            string_label(config.command.linkname.as_deref()),
        ),
        (
            "11. linkprotocol          ",
            string_label(config.command.linkprotocol.as_deref()),
        ),
        (
            "12. linkmode              ",
            string_label(config.command.linkmode.as_deref()),
        ),
        ("13. linkoptions           ", linkoptions.as_str()),
        (
            "14. linkinit              ",
            option_label(config.command.linkinit),
        ),
    ];
    let option_width = options
        .iter()
        .map(|(name, _)| name.chars().count())
        .max()
        .unwrap_or(0)
        .max("OPTION NAME".chars().count());
    let value_width = options
        .iter()
        .map(|(_, value)| value.chars().count())
        .max()
        .unwrap_or(0)
        .max("CURRENT VALUE".chars().count())
        + 1;
    let option_border = "─".repeat(option_width + 2);
    let value_border = "─".repeat(value_width + 2);

    println!("╭{}┬{}╮", option_border, value_border);
    println!(
        "│ {:<option_width$} │ {:<value_width$} │",
        "OPTION NAME", "CURRENT VALUE"
    );
    println!("├{}┼{}┤", option_border, value_border);
    options.into_iter().for_each(|(name, value)| {
        println!(
            "│ {} │ {} │",
            title_style.paint(format!("{name:<option_width$}")),
            value_style.paint(format!("{value:<value_width$}"))
        )
    });
    println!("╰{}┴{}╯", option_border, value_border);
    println!();
    println!("Commands:");
    println!(
        "{} - Edit option number {}",
        italic.paint("num"),
        italic.paint("num")
    );
    println!("e   - Open config file in $EDITOR");
    println!("p   - Print config file path");
    println!("q   - Quit settings editor");
}

fn option_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "on",
        Some(false) => "off",
        None => "default",
    }
}

fn string_label(value: Option<&str>) -> &str {
    value.filter(|value| !value.is_empty()).unwrap_or("default")
}

fn read_menu_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn configure_theme(config: &mut UserConfig, theme: &ThemeHandle, use_color: bool) -> Result<()> {
    println!("Available themes:");
    for (index, available_theme) in theme.registry().themes().iter().enumerate() {
        let marker = if *available_theme == theme.current() {
            "*"
        } else {
            " "
        };
        println!("  {:>2}. {marker} {}", index + 1, available_theme.name());
    }
    let input = read_menu_line("Theme name/number, or unset for default: ")?;
    if input.eq_ignore_ascii_case("unset") || input.eq_ignore_ascii_case("default") {
        config.theme = None;
        save_user_config(config)?;
        println!("Theme default cleared. Wolfie will use its built-in default next time.");
        return Ok(());
    }

    let selected = if let Ok(index) = input.parse::<usize>() {
        theme
            .registry()
            .themes()
            .get(index.saturating_sub(1))
            .cloned()
    } else {
        theme.registry().parse(&input)
    };
    let Some(selected) = selected else {
        println!("Unknown theme {input:?}. No change made.");
        return Ok(());
    };

    config.theme = Some(selected.name().to_string());
    save_user_config(config)?;
    if use_color || selected.is_plain() {
        theme.set(selected.clone())?;
        println!("Theme set to {}.", selected.name());
    } else {
        println!(
            "Theme saved as {}. Current session is still plain because color is disabled.",
            selected.name()
        );
    }
    Ok(())
}

fn configure_bool(
    config: &mut UserConfig,
    key: &str,
    description: &str,
    field: fn(&mut CommandConfig) -> &mut Option<bool>,
) -> Result<()> {
    println!("{key}: {description}");
    let input = read_menu_line("Set to on/off/default? ")?;
    let next = match input.trim().to_ascii_lowercase().as_str() {
        "on" | "yes" | "y" | "true" | "1" => Some(true),
        "off" | "no" | "n" | "false" | "0" => Some(false),
        "default" | "unset" | "clear" | "" => None,
        other => {
            println!("I don't understand {other:?}. No change made.");
            return Ok(());
        }
    };
    *field(&mut config.command) = next;
    save_user_config(config)?;
    println!("Saved {key} = {}.", option_label(next));
    Ok(())
}

fn configure_string(
    config: &mut UserConfig,
    key: &str,
    description: &str,
    field: fn(&mut CommandConfig) -> &mut Option<String>,
) -> Result<()> {
    println!("{key}: {description}");
    let input = read_menu_line("Enter a value, or unset for default: ")?;
    let next = if input.is_empty()
        || input.eq_ignore_ascii_case("unset")
        || input.eq_ignore_ascii_case("default")
    {
        None
    } else {
        Some(input)
    };
    *field(&mut config.command) = next.clone();
    save_user_config(config)?;
    println!("Saved {key} = {}.", string_label(next.as_deref()));
    Ok(())
}

fn configure_u32(
    config: &mut UserConfig,
    key: &str,
    description: &str,
    field: fn(&mut CommandConfig) -> &mut Option<u32>,
) -> Result<()> {
    println!("{key}: {description}");
    let input = read_menu_line("Enter a non-negative integer, or unset for default: ")?;
    let next = if input.is_empty()
        || input.eq_ignore_ascii_case("unset")
        || input.eq_ignore_ascii_case("default")
    {
        None
    } else {
        Some(
            input
                .parse::<u32>()
                .with_context(|| format!("{key} must be a non-negative integer"))?,
        )
    };
    *field(&mut config.command) = next;
    save_user_config(config)?;
    println!(
        "Saved {key} = {}.",
        next.map_or("default".to_string(), |value| value.to_string())
    );
    Ok(())
}

fn configure_link_protocol(config: &mut UserConfig) -> Result<()> {
    println!("linkprotocol: WSTP link protocol to use with linkconnect");
    println!("  1. SharedMemory");
    println!("  2. TCPIP");
    println!("  3. IntraProcess");
    let input = read_menu_line("Choose a protocol, or unset for default: ")?;
    let next = match input.trim().to_ascii_lowercase().as_str() {
        "1" | "sharedmemory" | "shared-memory" | "shared_memory" => {
            Some("SharedMemory".to_string())
        }
        "2" | "tcpip" | "tcp" => Some("TCPIP".to_string()),
        "3" | "intraprocess" | "intra-process" | "intra_process" => {
            Some("IntraProcess".to_string())
        }
        "unset" | "default" | "" => None,
        other => {
            println!("Unknown link protocol {other:?}. No change made.");
            return Ok(());
        }
    };
    config.command.linkprotocol = next.clone();
    save_user_config(config)?;
    println!("Saved linkprotocol = {}.", string_label(next.as_deref()));
    Ok(())
}

fn print_config_location() {
    match config_file() {
        Some(path) => println!("Config: {}", path.display()),
        None => println!("Could not determine the config file location."),
    }
}

pub(crate) fn run_shell_escape(command: &str) -> Result<ExitStatus> {
    shell_escape_command(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch shell")
}

pub(crate) fn top_level_run_command(input: &str) -> Option<String> {
    let mut rest = input.trim();
    rest = rest.strip_prefix("System`").unwrap_or(rest);
    rest = rest.strip_prefix("Run")?.trim_start();
    rest = rest.strip_prefix('[')?.trim_start();
    let (command, after_command) = parse_wolfram_string_literal(rest)?;
    let after_command = after_command.trim_start();
    after_command
        .strip_prefix(']')?
        .trim()
        .is_empty()
        .then_some(command)
}

fn parse_wolfram_string_literal(input: &str) -> Option<(String, &str)> {
    let mut chars = input.char_indices();
    let (_, opening) = chars.next()?;
    if opening != '"' {
        return None;
    }

    let mut value = String::new();
    let mut escaped = false;
    for (idx, ch) in chars {
        if escaped {
            match ch {
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                other => value.push(other),
            }
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some((value, &input[idx + ch.len_utf8()..])),
            other => value.push(other),
        }
    }

    None
}

#[cfg(unix)]
fn shell_escape_command(command: &str) -> Command {
    let shell = env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "sh".into());
    let mut child = Command::new(shell);
    if command.trim().is_empty() {
        child.arg("-i");
    } else {
        child.arg("-c").arg(command);
    }
    child
}

#[cfg(windows)]
fn shell_escape_command(command: &str) -> Command {
    let shell = env::var_os("COMSPEC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cmd.exe".into());
    let mut child = Command::new(shell);
    if !command.trim().is_empty() {
        child.arg("/C").arg(command);
    }
    child
}

fn edit_config_file() -> Result<()> {
    let path = config_file().context("could not determine the user config directory")?;
    ensure_config_file(&path)?;

    let editor = env::var_os("EDITOR")
        .filter(|value| !value.is_empty())
        .context("$EDITOR is not set")?;
    let status = run_editor(&editor, &path)?;
    if !status.success() {
        bail!("$EDITOR exited with {status}");
    }
    Ok(())
}

fn ensure_config_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    if !path.exists() {
        fs::write(path, default_config_content())
            .with_context(|| format!("failed to create config file {}", path.display()))?;
    }
    Ok(())
}

fn default_config_content() -> String {
    format!("{{\n  \"$schema\": \"{CONFIG_SCHEMA_URL}\"\n}}\n")
}

#[cfg(unix)]
fn run_editor(editor: &std::ffi::OsStr, path: &Path) -> Result<ExitStatus> {
    Command::new("sh")
        .arg("-c")
        .arg("exec $EDITOR \"$1\"")
        .arg("wolfie-config-edit")
        .arg(path)
        .env("EDITOR", editor)
        .status()
        .context("failed to launch $EDITOR")
}

#[cfg(windows)]
fn run_editor(editor: &std::ffi::OsStr, path: &Path) -> Result<ExitStatus> {
    Command::new(editor)
        .arg(path)
        .status()
        .context("failed to launch $EDITOR")
}

pub(crate) fn parse_repl_command(input: &str, registry: &ThemeRegistry) -> Result<ReplCommand> {
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
        "clear" | "cls" => {
            reject_extra_args(parts, ":clear")?;
            Ok(ReplCommand::Clear)
        }
        "config" | "conf" => match parts.next() {
            None => Ok(ReplCommand::Settings),
            Some("show" | "path" | "location") => {
                reject_extra_args(parts, ":config show")?;
                Ok(ReplCommand::Config(ConfigCommand::Show))
            }
            Some("edit") => {
                reject_extra_args(parts, ":config edit")?;
                Ok(ReplCommand::Config(ConfigCommand::Edit))
            }
            Some(value) => bail!("unknown config command {value:?}; usage: :config [show|edit]"),
        },
        "setting" | "settings" => {
            reject_extra_args(parts, ":setting")?;
            Ok(ReplCommand::Settings)
        }
        "help" | "h" | "?" => {
            reject_extra_args(parts, ":help")?;
            Ok(ReplCommand::Help)
        }
        "history" | "hist" => {
            reject_extra_args(parts, ":history")?;
            Ok(ReplCommand::History)
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
                let theme = registry
                    .parse(&theme_value)
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::Value;

    use super::*;

    #[test]
    fn creates_default_config_with_remote_schema() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "wolfie-config-test-{}-{unique}",
            std::process::id()
        ));
        let path = dir.join("config.json");

        ensure_config_file(&path).expect("config file should be created");

        let content = fs::read_to_string(&path).expect("config file should be readable");
        let json: Value = serde_json::from_str(&content).expect("config should be valid JSON");
        assert_eq!(json["$schema"], CONFIG_SCHEMA_URL);

        fs::write(&path, "{\"theme\":\"dark\"}\n").expect("config should be writable");
        ensure_config_file(&path).expect("existing config should be left in place");
        assert_eq!(
            fs::read_to_string(&path).expect("config file should be readable"),
            "{\"theme\":\"dark\"}\n"
        );

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn parses_top_level_run_string_for_shell_override() {
        assert_eq!(
            top_level_run_command(r#"Run["echo hello"]"#),
            Some("echo hello".to_string())
        );
        assert_eq!(
            top_level_run_command(r#"  System`Run["printf \"hi\""]  "#),
            Some("printf \"hi\"".to_string())
        );
        assert_eq!(
            top_level_run_command(r#"Run["printf a\nb"]"#),
            Some("printf a\nb".to_string())
        );
    }

    #[test]
    fn ignores_non_top_level_or_dynamic_run_inputs() {
        assert_eq!(top_level_run_command(r#"Run[cmd]"#), None);
        assert_eq!(top_level_run_command(r#"1 + Run["true"]"#), None);
        assert_eq!(top_level_run_command(r#"Run["true"] + 1"#), None);
        assert_eq!(top_level_run_command(r#"RunProcess[{"echo", "hi"}]"#), None);
    }
}
