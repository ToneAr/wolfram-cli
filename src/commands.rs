use std::{
    env, fs,
    io::{self, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
};

use anyhow::{Context, Result, bail};

use crate::theme::{
    CONFIG_SCHEMA_URL, Theme, ThemeHandle, ThemeRegistry, config_file, print_theme_browser,
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
    println!("  :config | :conf       Show config file location.");
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
            None => Ok(ReplCommand::Config(ConfigCommand::Show)),
            Some("edit") => {
                reject_extra_args(parts, ":config edit")?;
                Ok(ReplCommand::Config(ConfigCommand::Edit))
            }
            Some(value) => bail!("unknown config command {value:?}; usage: :config [edit]"),
        },
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
