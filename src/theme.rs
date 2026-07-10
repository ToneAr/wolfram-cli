use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use nu_ansi_term::{Color, Style};

use crate::highlighter::print_highlighted;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Theme {
    Dark = 0,
    Light = 1,
    Solarized = 2,
    Gruvbox = 3,
    Monokai = 4,
    Plain = 5,
}

impl Theme {
    pub(crate) const ALL: [Self; 6] = [
        Self::Dark,
        Self::Light,
        Self::Solarized,
        Self::Gruvbox,
        Self::Monokai,
        Self::Plain,
    ];

    pub(crate) fn parse(value: &str) -> Option<Self> {
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

    pub(crate) fn from_id(value: u8) -> Self {
        match value {
            1 => Self::Light,
            2 => Self::Solarized,
            3 => Self::Gruvbox,
            4 => Self::Monokai,
            5 => Self::Plain,
            _ => Self::Dark,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Solarized => "solarized",
            Self::Gruvbox => "gruvbox",
            Self::Monokai => "monokai",
            Self::Plain => "plain",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Solarized,
            Self::Solarized => Self::Gruvbox,
            Self::Gruvbox => Self::Monokai,
            Self::Monokai => Self::Plain,
            Self::Plain => Self::Dark,
        }
    }

    pub(crate) fn styles(self) -> ThemeStyles {
        match self {
            Self::Dark => ThemeStyles {
                string: Style::new().fg(Color::Green),
                comment: Style::new().fg(Color::DarkGray),
                number: Style::new().fg(Color::Purple),
                builtin_symbol: Style::new().fg(Color::Cyan).bold(),
                user_symbol: Style::new().fg(Color::LightGreen),
                completion_command: Style::new().fg(Color::LightBlue),
                completion_symbol: Style::new().fg(Color::Cyan),
                completion_user_symbol: Style::new().fg(Color::LightGreen),
                completion_context: Style::new().fg(Color::Yellow),
                completion_option: Style::new().fg(Color::Yellow),
            },
            Self::Light => ThemeStyles {
                string: Style::new().fg(Color::Fixed(28)),
                comment: Style::new().fg(Color::Fixed(244)),
                number: Style::new().fg(Color::Fixed(90)),
                builtin_symbol: Style::new().fg(Color::Fixed(25)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(94)),
                completion_command: Style::new().fg(Color::Fixed(25)),
                completion_symbol: Style::new().fg(Color::Fixed(25)),
                completion_user_symbol: Style::new().fg(Color::Fixed(94)),
                completion_context: Style::new().fg(Color::Fixed(130)),
                completion_option: Style::new().fg(Color::Fixed(130)),
            },
            Self::Solarized => ThemeStyles {
                string: Style::new().fg(Color::Fixed(64)),
                comment: Style::new().fg(Color::Fixed(244)).italic(),
                number: Style::new().fg(Color::Fixed(136)),
                builtin_symbol: Style::new().fg(Color::Fixed(33)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(37)),
                completion_command: Style::new().fg(Color::Fixed(33)),
                completion_symbol: Style::new().fg(Color::Fixed(33)),
                completion_user_symbol: Style::new().fg(Color::Fixed(37)),
                completion_context: Style::new().fg(Color::Fixed(136)),
                completion_option: Style::new().fg(Color::Fixed(136)),
            },
            Self::Gruvbox => ThemeStyles {
                string: Style::new().fg(Color::Fixed(142)),
                comment: Style::new().fg(Color::Fixed(245)).italic(),
                number: Style::new().fg(Color::Fixed(208)),
                builtin_symbol: Style::new().fg(Color::Fixed(109)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(175)),
                completion_command: Style::new().fg(Color::Fixed(109)),
                completion_symbol: Style::new().fg(Color::Fixed(109)),
                completion_user_symbol: Style::new().fg(Color::Fixed(175)),
                completion_context: Style::new().fg(Color::Fixed(214)),
                completion_option: Style::new().fg(Color::Fixed(214)),
            },
            Self::Monokai => ThemeStyles {
                string: Style::new().fg(Color::Fixed(148)),
                comment: Style::new().fg(Color::Fixed(59)).italic(),
                number: Style::new().fg(Color::Fixed(141)),
                builtin_symbol: Style::new().fg(Color::Fixed(81)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(197)),
                completion_command: Style::new().fg(Color::Fixed(81)),
                completion_symbol: Style::new().fg(Color::Fixed(81)),
                completion_user_symbol: Style::new().fg(Color::Fixed(197)),
                completion_context: Style::new().fg(Color::Fixed(186)),
                completion_option: Style::new().fg(Color::Fixed(186)),
            },
            Self::Plain => ThemeStyles {
                string: Style::new(),
                comment: Style::new(),
                number: Style::new(),
                builtin_symbol: Style::new(),
                user_symbol: Style::new(),
                completion_command: Style::new(),
                completion_symbol: Style::new(),
                completion_user_symbol: Style::new(),
                completion_context: Style::new(),
                completion_option: Style::new(),
            },
        }
    }
}

pub(crate) fn print_theme_browser(current: Theme, use_color: bool) {
    println!("Themes:");
    for theme in Theme::ALL {
        let marker = if theme == current { "*" } else { " " };
        print!("  {marker} {:<9} ", theme.name());
        let preview_theme = if use_color { theme } else { Theme::Plain };
        print_highlighted(
            "Plot[Sin[x], {x, 0, 2 Pi}] (* sample *) \"text\" 42",
            preview_theme,
        );
    }
}

#[derive(Clone)]
pub(crate) struct ThemeHandle {
    current: Arc<AtomicU64>,
}

impl ThemeHandle {
    pub(crate) fn new(theme: Theme) -> Self {
        Self {
            current: Arc::new(AtomicU64::new(theme as u64)),
        }
    }

    pub(crate) fn current(&self) -> Theme {
        Theme::from_id(self.current.load(Ordering::Relaxed) as u8)
    }

    pub(crate) fn set(&self, theme: Theme) {
        self.current.store(theme as u64, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ThemeStyles {
    pub(crate) string: Style,
    pub(crate) comment: Style,
    pub(crate) number: Style,
    pub(crate) builtin_symbol: Style,
    pub(crate) user_symbol: Style,
    pub(crate) completion_command: Style,
    pub(crate) completion_symbol: Style,
    pub(crate) completion_user_symbol: Style,
    pub(crate) completion_context: Style,
    pub(crate) completion_option: Style,
}
