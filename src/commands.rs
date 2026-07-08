use std::io::{self, Write};

use anyhow::{Context, Result, bail};

use crate::theme::{Theme, ThemeHandle, print_theme_browser};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandAction {
    Continue,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplCommand {
    Clear,
    Help,
    Theme(ThemeCommand),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ThemeCommand {
    Cycle,
    List,
    Show,
    Set(Theme),
}

pub(crate) fn execute_repl_command(input: &str, theme: &ThemeHandle) -> CommandAction {
    let command = match parse_repl_command(input) {
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
    println!("  :clear                Clear the console.");
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

pub(crate) fn parse_repl_command(input: &str) -> Result<ReplCommand> {
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
