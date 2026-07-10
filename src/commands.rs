use std::io::{self, Write};

use anyhow::{Context, Result, bail};

use crate::theme::{Theme, ThemeHandle, ThemeRegistry, print_theme_browser};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAction {
    Continue,
    OpenHistory,
    Quit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReplCommand {
    Clear,
    Help,
    History,
    Theme(ThemeCommand),
    Quit,
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

fn print_command_help(theme: Theme, registry: &ThemeRegistry) {
    println!("Commands:");
    println!("  :clear                Clear the console.");
    println!("  :help | :h | :?       Show this help.");
    println!("  :history | :hist      Open the history browser.");
    println!("                        Press a key to search/navigate.");
    println!("                        Can also be opened using CTRL+r");
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
