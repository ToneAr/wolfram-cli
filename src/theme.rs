use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result, anyhow, bail};
use nu_ansi_term::{Color, Style};
use serde::{Deserialize, Serialize};

use crate::highlighter::print_highlighted;

pub(crate) const CONFIG_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/ToneAr/wolfish/main/schemas/config.schema.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuiltinTheme {
    Dark,
    Light,
    Solarized,
    Gruvbox,
    Monokai,
    Plain,
}

impl BuiltinTheme {
    pub(crate) const ALL: [Self; 6] = [
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

    pub(crate) fn styles(self) -> ThemeStyles {
        match self {
            Self::Dark => ThemeStyles {
                string: Style::new().fg(Color::Fixed(208)),
                comment: Style::new().fg(Color::Fixed(131)).italic(),
                number: Style::new().fg(Color::Fixed(202)),
                builtin_symbol: Style::new().fg(Color::Fixed(203)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(214)),
                completion_command: Style::new().fg(Color::Fixed(202)).bold(),
                completion_symbol: Style::new().fg(Color::Fixed(203)),
                completion_global_symbol: Style::new().fg(Color::Fixed(209)),
                completion_user_symbol: Style::new().fg(Color::Fixed(214)),
                completion_context: Style::new().fg(Color::Fixed(208)).bold(),
                completion_option: Style::new().fg(Color::Fixed(166)),
                completion_directory: Style::new().fg(Color::Fixed(215)).bold(),
                completion_file: Style::new().fg(Color::Fixed(173)),
                menu_text: Style::new().fg(Color::Fixed(215)),
                menu_selected_text: Style::new().fg(Color::White).on(Color::Fixed(160)).bold(),
                menu_description: Style::new().fg(Color::Fixed(166)),
                menu_match: Style::new().fg(Color::Fixed(208)).underline(),
                menu_selected_match: Style::new()
                    .fg(Color::Fixed(226))
                    .on(Color::Fixed(160))
                    .bold()
                    .underline(),
                visual_selection: Style::new().fg(Color::White).on(Color::Fixed(88)),
                prompt_left: Style::new().fg(Color::Fixed(202)).bold(),
                prompt_multiline_text: Style::new().fg(Color::Fixed(208)).bold(),
                prompt_right_text: Style::new().fg(Color::Fixed(166)).bold(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
            Self::Light => ThemeStyles {
                string: Style::new().fg(Color::Fixed(130)),
                comment: Style::new().fg(Color::Fixed(131)).italic(),
                number: Style::new().fg(Color::Fixed(166)),
                builtin_symbol: Style::new().fg(Color::Fixed(124)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(136)),
                completion_command: Style::new().fg(Color::Fixed(160)).bold(),
                completion_symbol: Style::new().fg(Color::Fixed(124)),
                completion_global_symbol: Style::new().fg(Color::Fixed(160)),
                completion_user_symbol: Style::new().fg(Color::Fixed(136)),
                completion_context: Style::new().fg(Color::Fixed(166)).bold(),
                completion_option: Style::new().fg(Color::Fixed(130)),
                completion_directory: Style::new().fg(Color::Fixed(172)).bold(),
                completion_file: Style::new().fg(Color::Fixed(94)),
                menu_text: Style::new().fg(Color::Fixed(88)),
                menu_selected_text: Style::new().fg(Color::White).on(Color::Fixed(166)).bold(),
                menu_description: Style::new().fg(Color::Fixed(130)),
                menu_match: Style::new().fg(Color::Fixed(160)).underline(),
                menu_selected_match: Style::new()
                    .fg(Color::Fixed(226))
                    .on(Color::Fixed(166))
                    .bold()
                    .underline(),
                visual_selection: Style::new().fg(Color::White).on(Color::Fixed(166)),
                prompt_left: Style::new().fg(Color::Fixed(124)).bold(),
                prompt_multiline_text: Style::new().fg(Color::Fixed(166)).bold(),
                prompt_right_text: Style::new().fg(Color::Fixed(130)).bold(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
            Self::Solarized => ThemeStyles {
                string: Style::new().fg(Color::Fixed(64)),
                comment: Style::new().fg(Color::Fixed(244)).italic(),
                number: Style::new().fg(Color::Fixed(136)),
                builtin_symbol: Style::new().fg(Color::Fixed(33)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(37)),
                completion_command: Style::new().fg(Color::Fixed(33)),
                completion_symbol: Style::new().fg(Color::Fixed(33)),
                completion_global_symbol: Style::new().fg(Color::Fixed(38)),
                completion_user_symbol: Style::new().fg(Color::Fixed(37)),
                completion_context: Style::new().fg(Color::Fixed(136)).bold(),
                completion_option: Style::new().fg(Color::Fixed(136)),
                completion_directory: Style::new().fg(Color::Fixed(64)).bold(),
                completion_file: Style::new().fg(Color::Fixed(66)),
                menu_text: Style::new().fg(Color::Fixed(37)),
                menu_selected_text: Style::new()
                    .fg(Color::Fixed(230))
                    .on(Color::Fixed(33))
                    .bold(),
                menu_description: Style::new().fg(Color::Fixed(136)),
                menu_match: Style::new().fg(Color::Fixed(136)).underline(),
                menu_selected_match: Style::new()
                    .fg(Color::Fixed(230))
                    .on(Color::Fixed(33))
                    .bold()
                    .underline(),
                visual_selection: Style::new().fg(Color::Fixed(230)).on(Color::Fixed(33)),
                prompt_left: Style::new().fg(Color::Fixed(33)).bold(),
                prompt_multiline_text: Style::new().fg(Color::Fixed(136)).bold(),
                prompt_right_text: Style::new().fg(Color::Fixed(136)).bold(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
            Self::Gruvbox => ThemeStyles {
                string: Style::new().fg(Color::Fixed(142)),
                comment: Style::new().fg(Color::Fixed(245)).italic(),
                number: Style::new().fg(Color::Fixed(208)),
                builtin_symbol: Style::new().fg(Color::Fixed(109)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(175)),
                completion_command: Style::new().fg(Color::Fixed(109)),
                completion_symbol: Style::new().fg(Color::Fixed(109)),
                completion_global_symbol: Style::new().fg(Color::Fixed(208)),
                completion_user_symbol: Style::new().fg(Color::Fixed(175)),
                completion_context: Style::new().fg(Color::Fixed(214)).bold(),
                completion_option: Style::new().fg(Color::Fixed(214)),
                completion_directory: Style::new().fg(Color::Fixed(142)).bold(),
                completion_file: Style::new().fg(Color::Fixed(223)),
                menu_text: Style::new().fg(Color::Fixed(223)),
                menu_selected_text: Style::new()
                    .fg(Color::Fixed(235))
                    .on(Color::Fixed(214))
                    .bold(),
                menu_description: Style::new().fg(Color::Fixed(214)),
                menu_match: Style::new().fg(Color::Fixed(208)).underline(),
                menu_selected_match: Style::new()
                    .fg(Color::Fixed(235))
                    .on(Color::Fixed(214))
                    .bold()
                    .underline(),
                visual_selection: Style::new().fg(Color::Fixed(235)).on(Color::Fixed(214)),
                prompt_left: Style::new().fg(Color::Fixed(214)).bold(),
                prompt_multiline_text: Style::new().fg(Color::Fixed(208)).bold(),
                prompt_right_text: Style::new().fg(Color::Fixed(142)).bold(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
            Self::Monokai => ThemeStyles {
                string: Style::new().fg(Color::Fixed(148)),
                comment: Style::new().fg(Color::Fixed(59)).italic(),
                number: Style::new().fg(Color::Fixed(141)),
                builtin_symbol: Style::new().fg(Color::Fixed(81)).bold(),
                user_symbol: Style::new().fg(Color::Fixed(197)),
                completion_command: Style::new().fg(Color::Fixed(81)),
                completion_symbol: Style::new().fg(Color::Fixed(81)),
                completion_global_symbol: Style::new().fg(Color::Fixed(204)),
                completion_user_symbol: Style::new().fg(Color::Fixed(197)),
                completion_context: Style::new().fg(Color::Fixed(186)).bold(),
                completion_option: Style::new().fg(Color::Fixed(186)),
                completion_directory: Style::new().fg(Color::Fixed(148)).bold(),
                completion_file: Style::new().fg(Color::Fixed(231)),
                menu_text: Style::new().fg(Color::Fixed(231)),
                menu_selected_text: Style::new()
                    .fg(Color::Fixed(232))
                    .on(Color::Fixed(186))
                    .bold(),
                menu_description: Style::new().fg(Color::Fixed(186)),
                menu_match: Style::new().fg(Color::Fixed(197)).underline(),
                menu_selected_match: Style::new()
                    .fg(Color::Fixed(232))
                    .on(Color::Fixed(186))
                    .bold()
                    .underline(),
                visual_selection: Style::new().fg(Color::Fixed(232)).on(Color::Fixed(186)),
                prompt_left: Style::new().fg(Color::Fixed(186)).bold(),
                prompt_multiline_text: Style::new().fg(Color::Fixed(197)).bold(),
                prompt_right_text: Style::new().fg(Color::Fixed(141)).bold(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
            Self::Plain => ThemeStyles {
                string: Style::new(),
                comment: Style::new(),
                number: Style::new(),
                builtin_symbol: Style::new(),
                user_symbol: Style::new(),
                completion_command: Style::new(),
                completion_symbol: Style::new(),
                completion_global_symbol: Style::new(),
                completion_user_symbol: Style::new(),
                completion_context: Style::new(),
                completion_option: Style::new(),
                completion_directory: Style::new(),
                completion_file: Style::new(),
                menu_text: Style::new(),
                menu_selected_text: Style::new().reverse(),
                menu_description: Style::new(),
                menu_match: Style::new().underline(),
                menu_selected_match: Style::new().reverse().underline(),
                visual_selection: Style::new().reverse(),
                prompt_left: Style::new(),
                prompt_multiline_text: Style::new(),
                prompt_right_text: Style::new(),
                prompt: crossterm::style::Color::Reset,
                prompt_multiline: Color::White,
                prompt_indicator: crossterm::style::Color::Reset,
                prompt_right: crossterm::style::Color::Reset,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Theme {
    Builtin(BuiltinTheme),
    Custom(Arc<CustomTheme>),
}

impl PartialEq for Theme {
    fn eq(&self, other: &Self) -> bool {
        self.name().eq_ignore_ascii_case(other.name())
    }
}

impl Eq for Theme {}

impl Theme {
    pub(crate) fn builtin(theme: BuiltinTheme) -> Self {
        Self::Builtin(theme)
    }

    pub(crate) fn dark() -> Self {
        Self::Builtin(BuiltinTheme::Dark)
    }

    pub(crate) fn plain() -> Self {
        Self::Builtin(BuiltinTheme::Plain)
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::Builtin(theme) => theme.name(),
            Self::Custom(theme) => &theme.name,
        }
    }

    pub(crate) fn styles(&self) -> ThemeStyles {
        match self {
            Self::Builtin(theme) => theme.styles(),
            Self::Custom(theme) => theme.styles,
        }
    }

    pub(crate) fn is_plain(&self) -> bool {
        matches!(self, Self::Builtin(BuiltinTheme::Plain))
    }
}

#[derive(Debug)]
pub(crate) struct CustomTheme {
    name: String,
    styles: ThemeStyles,
}

#[derive(Clone)]
pub(crate) struct ThemeRegistry {
    themes: Arc<Vec<Theme>>,
}

impl ThemeRegistry {
    pub(crate) fn load() -> Self {
        let mut themes: Vec<Theme> = BuiltinTheme::ALL.into_iter().map(Theme::builtin).collect();
        let mut names = themes
            .iter()
            .map(|theme| theme.name().to_ascii_lowercase())
            .collect::<std::collections::HashSet<_>>();

        for theme in load_custom_themes() {
            let normalized = theme.name().to_ascii_lowercase();
            if !names.insert(normalized) {
                eprintln!(
                    "Wolfish::theme: ignoring custom theme {:?}; a theme with that name already exists",
                    theme.name()
                );
                continue;
            }
            themes.push(theme);
        }

        Self {
            themes: Arc::new(themes),
        }
    }

    pub(crate) fn builtin_only() -> Self {
        Self {
            themes: Arc::new(BuiltinTheme::ALL.into_iter().map(Theme::builtin).collect()),
        }
    }

    pub(crate) fn parse(&self, value: &str) -> Option<Theme> {
        let normalized = value.trim().to_ascii_lowercase();
        if let Some(theme) = BuiltinTheme::parse(&normalized) {
            return Some(Theme::builtin(theme));
        }

        self.themes
            .iter()
            .find(|theme| theme.name().eq_ignore_ascii_case(&normalized))
            .cloned()
    }

    pub(crate) fn next_after(&self, current: &Theme) -> Theme {
        let Some(index) = self.themes.iter().position(|theme| theme == current) else {
            return self.themes.first().cloned().unwrap_or_else(Theme::dark);
        };
        self.themes[(index + 1) % self.themes.len()].clone()
    }

    pub(crate) fn themes(&self) -> &[Theme] {
        self.themes.as_ref().as_slice()
    }
}

pub(crate) fn selected_theme(use_color: bool, registry: &ThemeRegistry) -> Theme {
    if !use_color {
        return Theme::plain();
    }

    load_theme_preference()
        .and_then(|name| registry.parse(&name))
        .unwrap_or_else(Theme::dark)
}

pub(crate) fn config_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .or_else(|| {
            env::var_os("APPDATA")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| dir.join("wolfish"))
}

pub(crate) fn custom_theme_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("themes"))
}

pub(crate) fn config_file() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.json"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct UserConfig {
    #[serde(
        rename = "$schema",
        skip_serializing_if = "Option::is_none",
        default = "default_config_schema"
    )]
    pub(crate) schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) theme: Option<String>,
    #[serde(skip_serializing_if = "CommandConfig::is_empty")]
    pub(crate) command: CommandConfig,
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            schema: default_config_schema(),
            theme: None,
            command: CommandConfig::default(),
        }
    }
}

fn default_config_schema() -> Option<String> {
    Some(CONFIG_SCHEMA_URL.to_string())
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct CommandConfig {
    #[serde(
        rename = "no-frontend",
        alias = "no_frontend",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) no_frontend: Option<bool>,
    #[serde(
        rename = "no-color",
        alias = "no_color",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) no_color: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linkconnect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linkname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linkprotocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linkoptions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) linkmode: Option<String>,
}

impl CommandConfig {
    fn is_empty(&self) -> bool {
        self.no_frontend.is_none()
            && self.no_color.is_none()
            && self.linkconnect.is_none()
            && self.linkname.is_none()
            && self.linkprotocol.is_none()
            && self.linkoptions.is_none()
            && self.linkmode.is_none()
    }
}

pub(crate) fn load_user_config() -> UserConfig {
    let Some(path) = config_file() else {
        return UserConfig::default();
    };
    let Ok(content) = fs::read_to_string(path) else {
        return UserConfig::default();
    };
    serde_json::from_str::<UserConfig>(&content).unwrap_or_default()
}

fn load_theme_preference() -> Option<String> {
    load_user_config().theme
}

fn save_theme_preference(theme: &Theme) -> Result<()> {
    let path = config_file().context("could not determine the user config directory")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    let mut config = load_user_config();
    config.theme = Some(theme.name().to_string());
    let content = serde_json::to_string_pretty(&config)?;
    fs::write(&path, format!("{content}\n"))
        .with_context(|| format!("failed to write theme preference to {}", path.display()))
}

pub(crate) fn print_theme_browser(current: Theme, registry: &ThemeRegistry, use_color: bool) {
    println!("Themes:");
    for theme in registry.themes() {
        let marker = if *theme == current { "*" } else { " " };
        let preview_theme = if use_color {
            theme.clone()
        } else {
            Theme::plain()
        };
        let styles = preview_theme.styles();
        print!("  {marker} {:<16} ", theme.name());
        print!("{} ", styles.prompt_left.paint("In[1]:="));
        print_highlighted(
            "Plot[Sin[x], {x, 0, 2 Pi}] (* comment *) \"text\" 42",
            &preview_theme,
        );
    }

    if let Some(path) = custom_theme_dir() {
        println!("Custom themes: {}/*.json", path.display());
    }
}

#[derive(Clone)]
pub(crate) struct ThemeHandle {
    current: Arc<RwLock<Theme>>,
    registry: ThemeRegistry,
    persist_selection: bool,
}

impl ThemeHandle {
    pub(crate) fn new(theme: Theme, registry: ThemeRegistry) -> Self {
        Self {
            current: Arc::new(RwLock::new(theme)),
            registry,
            persist_selection: true,
        }
    }

    pub(crate) fn builtin(theme: Theme) -> Self {
        Self {
            current: Arc::new(RwLock::new(theme)),
            registry: ThemeRegistry::builtin_only(),
            persist_selection: false,
        }
    }

    pub(crate) fn current(&self) -> Theme {
        self.current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn registry(&self) -> &ThemeRegistry {
        &self.registry
    }

    pub(crate) fn next(&self) -> Theme {
        self.registry.next_after(&self.current())
    }

    pub(crate) fn set(&self, theme: Theme) -> Result<()> {
        {
            let mut current = self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *current = theme.clone();
        }
        if self.persist_selection {
            save_theme_preference(&theme)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ThemeStyles {
    pub(crate) string: Style,
    pub(crate) comment: Style,
    pub(crate) number: Style,
    pub(crate) builtin_symbol: Style,
    pub(crate) user_symbol: Style,
    pub(crate) completion_command: Style,
    pub(crate) completion_symbol: Style,
    pub(crate) completion_global_symbol: Style,
    pub(crate) completion_user_symbol: Style,
    pub(crate) completion_context: Style,
    pub(crate) completion_option: Style,
    pub(crate) completion_directory: Style,
    pub(crate) completion_file: Style,
    pub(crate) menu_text: Style,
    pub(crate) menu_selected_text: Style,
    pub(crate) menu_description: Style,
    pub(crate) menu_match: Style,
    pub(crate) menu_selected_match: Style,
    pub(crate) visual_selection: Style,
    pub(crate) prompt_left: Style,
    pub(crate) prompt_multiline_text: Style,
    pub(crate) prompt_right_text: Style,
    pub(crate) prompt: crossterm::style::Color,
    pub(crate) prompt_multiline: Color,
    pub(crate) prompt_indicator: crossterm::style::Color,
    pub(crate) prompt_right: crossterm::style::Color,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonTheme {
    name: Option<String>,
    base: Option<String>,
    styles: Option<JsonThemeStyles>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct JsonThemeStyles {
    string: Option<JsonStyle>,
    comment: Option<JsonStyle>,
    number: Option<JsonStyle>,
    builtin_symbol: Option<JsonStyle>,
    user_symbol: Option<JsonStyle>,
    completion_command: Option<JsonStyle>,
    completion_symbol: Option<JsonStyle>,
    completion_global_symbol: Option<JsonStyle>,
    completion_user_symbol: Option<JsonStyle>,
    completion_context: Option<JsonStyle>,
    completion_option: Option<JsonStyle>,
    completion_directory: Option<JsonStyle>,
    completion_file: Option<JsonStyle>,
    menu_text: Option<JsonStyle>,
    menu_selected_text: Option<JsonStyle>,
    menu_description: Option<JsonStyle>,
    menu_match: Option<JsonStyle>,
    menu_selected_match: Option<JsonStyle>,
    visual_selection: Option<JsonStyle>,
    prompt_left: Option<JsonStyle>,
    prompt_multiline_text: Option<JsonStyle>,
    prompt_right_text: Option<JsonStyle>,
    prompt: Option<JsonColor>,
    prompt_multiline: Option<JsonColor>,
    prompt_indicator: Option<JsonColor>,
    prompt_right: Option<JsonColor>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum JsonColor {
    Fixed(u8),
    Rgb([u8; 3]),
    Named(String),
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum JsonStyle {
    Foreground(JsonColor),
    Object(JsonStyleObject),
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct JsonStyleObject {
    fg: Option<JsonColor>,
    bg: Option<JsonColor>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    reverse: Option<bool>,
}

fn load_custom_themes() -> Vec<Theme> {
    let Some(dir) = custom_theme_dir() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut paths = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();

    paths
        .into_iter()
        .filter_map(|path| match load_custom_theme(&path) {
            Ok(theme) => Some(theme),
            Err(err) => {
                eprintln!(
                    "Wolfish::theme: failed to load custom theme {}: {err:#}",
                    path.display()
                );
                None
            }
        })
        .collect()
}

fn load_custom_theme(path: &Path) -> Result<Theme> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read theme file {}", path.display()))?;
    let json: JsonTheme = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse theme JSON in {}", path.display()))?;
    let fallback_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("theme filename is not valid UTF-8")?;
    let name = json
        .name
        .as_deref()
        .unwrap_or(fallback_name)
        .trim()
        .to_string();

    if name.is_empty() {
        bail!("theme name cannot be empty");
    }
    if name.chars().any(char::is_whitespace) {
        bail!("theme name {name:?} cannot contain whitespace");
    }

    let base = match json.base.as_deref() {
        Some(base) => BuiltinTheme::parse(&base.to_ascii_lowercase())
            .with_context(|| format!("unknown base theme {base:?}"))?,
        None => BuiltinTheme::Dark,
    };
    let mut styles = base.styles();
    if let Some(overrides) = json.styles {
        overrides.apply_to(&mut styles)?;
    }

    Ok(Theme::Custom(Arc::new(CustomTheme { name, styles })))
}

impl JsonThemeStyles {
    fn apply_to(self, styles: &mut ThemeStyles) -> Result<()> {
        apply_style(&mut styles.string, self.string, "string")?;
        apply_style(&mut styles.comment, self.comment, "comment")?;
        apply_style(&mut styles.number, self.number, "number")?;
        apply_style(
            &mut styles.builtin_symbol,
            self.builtin_symbol,
            "builtin_symbol",
        )?;
        apply_style(&mut styles.user_symbol, self.user_symbol, "user_symbol")?;
        apply_style(
            &mut styles.completion_command,
            self.completion_command,
            "completion_command",
        )?;
        apply_style(
            &mut styles.completion_symbol,
            self.completion_symbol,
            "completion_symbol",
        )?;
        apply_style(
            &mut styles.completion_global_symbol,
            self.completion_global_symbol,
            "completion_global_symbol",
        )?;
        apply_style(
            &mut styles.completion_user_symbol,
            self.completion_user_symbol,
            "completion_user_symbol",
        )?;
        apply_style(
            &mut styles.completion_context,
            self.completion_context,
            "completion_context",
        )?;
        apply_style(
            &mut styles.completion_option,
            self.completion_option,
            "completion_option",
        )?;
        apply_style(
            &mut styles.completion_directory,
            self.completion_directory,
            "completion_directory",
        )?;
        apply_style(
            &mut styles.completion_file,
            self.completion_file,
            "completion_file",
        )?;
        apply_style(&mut styles.menu_text, self.menu_text, "menu_text")?;
        apply_style(
            &mut styles.menu_selected_text,
            self.menu_selected_text,
            "menu_selected_text",
        )?;
        apply_style(
            &mut styles.menu_description,
            self.menu_description,
            "menu_description",
        )?;
        apply_style(&mut styles.menu_match, self.menu_match, "menu_match")?;
        apply_style(
            &mut styles.menu_selected_match,
            self.menu_selected_match,
            "menu_selected_match",
        )?;
        apply_style(
            &mut styles.visual_selection,
            self.visual_selection,
            "visual_selection",
        )?;
        apply_style(&mut styles.prompt_left, self.prompt_left, "prompt_left")?;
        apply_style(
            &mut styles.prompt_multiline_text,
            self.prompt_multiline_text,
            "prompt_multiline_text",
        )?;
        apply_style(
            &mut styles.prompt_right_text,
            self.prompt_right_text,
            "prompt_right_text",
        )?;

        if let Some(color) = self.prompt {
            styles.prompt = color.to_crossterm_color()?;
        }
        if let Some(color) = self.prompt_multiline {
            styles.prompt_multiline = color.to_ansi_color()?;
        }
        if let Some(color) = self.prompt_indicator {
            styles.prompt_indicator = color.to_crossterm_color()?;
        }
        if let Some(color) = self.prompt_right {
            styles.prompt_right = color.to_crossterm_color()?;
        }

        Ok(())
    }
}

fn apply_style(target: &mut Style, style: Option<JsonStyle>, field: &str) -> Result<()> {
    let Some(style) = style else {
        return Ok(());
    };
    *target = style
        .apply_to(*target)
        .with_context(|| format!("invalid style override for {field}"))?;
    Ok(())
}

impl JsonStyle {
    fn apply_to(&self, mut style: Style) -> Result<Style> {
        match self {
            Self::Foreground(color) => Ok(style.fg(color.to_ansi_color()?)),
            Self::Object(object) => {
                if let Some(color) = &object.fg {
                    style = style.fg(color.to_ansi_color()?);
                }
                if let Some(color) = &object.bg {
                    style = style.on(color.to_ansi_color()?);
                }
                if object.bold.unwrap_or(false) {
                    style = style.bold();
                }
                if object.italic.unwrap_or(false) {
                    style = style.italic();
                }
                if object.underline.unwrap_or(false) {
                    style = style.underline();
                }
                if object.reverse.unwrap_or(false) {
                    style = style.reverse();
                }
                Ok(style)
            }
        }
    }
}

impl JsonColor {
    fn to_ansi_color(&self) -> Result<Color> {
        match self {
            Self::Fixed(value) => Ok(Color::Fixed(*value)),
            Self::Rgb([r, g, b]) => Ok(Color::Rgb(*r, *g, *b)),
            Self::Named(value) => parse_ansi_color(value),
        }
    }

    fn to_crossterm_color(&self) -> Result<crossterm::style::Color> {
        match self {
            Self::Fixed(value) => Ok(crossterm::style::Color::AnsiValue(*value)),
            Self::Rgb([r, g, b]) => Ok(crossterm::style::Color::Rgb {
                r: *r,
                g: *g,
                b: *b,
            }),
            Self::Named(value) => parse_crossterm_color(value),
        }
    }
}

fn parse_ansi_color(value: &str) -> Result<Color> {
    if let Some((r, g, b)) = parse_rgb_hex(value) {
        return Ok(Color::Rgb(r, g, b));
    }
    if let Ok(value) = value.parse::<u8>() {
        return Ok(Color::Fixed(value));
    }

    match normalize_color_name(value).as_str() {
        "black" => Ok(Color::Black),
        "red" => Ok(Color::Red),
        "green" => Ok(Color::Green),
        "yellow" => Ok(Color::Yellow),
        "blue" => Ok(Color::Blue),
        "purple" | "magenta" => Ok(Color::Purple),
        "cyan" => Ok(Color::Cyan),
        "white" => Ok(Color::White),
        "gray" | "grey" | "darkgray" | "darkgrey" => Ok(Color::Fixed(8)),
        "brightblack" => Ok(Color::Fixed(8)),
        "brightred" => Ok(Color::Fixed(9)),
        "brightgreen" => Ok(Color::Fixed(10)),
        "brightyellow" => Ok(Color::Fixed(11)),
        "brightblue" => Ok(Color::Fixed(12)),
        "brightpurple" | "brightmagenta" => Ok(Color::Fixed(13)),
        "brightcyan" => Ok(Color::Fixed(14)),
        "brightwhite" => Ok(Color::Fixed(15)),
        "reset" => bail!("reset is only valid for prompt color fields"),
        _ => Err(anyhow!("unknown color {value:?}")),
    }
}

fn parse_crossterm_color(value: &str) -> Result<crossterm::style::Color> {
    if let Some((r, g, b)) = parse_rgb_hex(value) {
        return Ok(crossterm::style::Color::Rgb { r, g, b });
    }
    if let Ok(value) = value.parse::<u8>() {
        return Ok(crossterm::style::Color::AnsiValue(value));
    }

    match normalize_color_name(value).as_str() {
        "reset" => Ok(crossterm::style::Color::Reset),
        "black" => Ok(crossterm::style::Color::Black),
        "red" => Ok(crossterm::style::Color::DarkRed),
        "green" => Ok(crossterm::style::Color::DarkGreen),
        "yellow" => Ok(crossterm::style::Color::DarkYellow),
        "blue" => Ok(crossterm::style::Color::DarkBlue),
        "purple" | "magenta" => Ok(crossterm::style::Color::DarkMagenta),
        "cyan" => Ok(crossterm::style::Color::DarkCyan),
        "white" => Ok(crossterm::style::Color::White),
        "gray" | "grey" => Ok(crossterm::style::Color::Grey),
        "darkgray" | "darkgrey" | "brightblack" => Ok(crossterm::style::Color::DarkGrey),
        "brightred" => Ok(crossterm::style::Color::Red),
        "brightgreen" => Ok(crossterm::style::Color::Green),
        "brightyellow" => Ok(crossterm::style::Color::Yellow),
        "brightblue" => Ok(crossterm::style::Color::Blue),
        "brightpurple" | "brightmagenta" => Ok(crossterm::style::Color::Magenta),
        "brightcyan" => Ok(crossterm::style::Color::Cyan),
        "brightwhite" => Ok(crossterm::style::Color::White),
        _ => Err(anyhow!("unknown color {value:?}")),
    }
}

fn normalize_color_name(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, '-' | '_' | ' '))
        .collect()
}

fn parse_rgb_hex(value: &str) -> Option<(u8, u8, u8)> {
    let value = value.trim().strip_prefix('#')?;
    if value.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&value[0..2], 16).ok()?;
    let g = u8::from_str_radix(&value[2..4], 16).ok()?;
    let b = u8::from_str_radix(&value[4..6], 16).ok()?;
    Some((r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_theme_file(name: &str, content: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "wolfish-theme-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_custom_json_theme() {
        let path = temp_theme_file(
            "custom.json",
            r##"{
                "name": "custom-theme",
                "base": "plain",
                "styles": {
                    "string": "#ff8000",
                    "comment": { "fg": 244, "italic": true },
                    "visual_selection": { "fg": "white", "bg": "#5f0000" },
                    "prompt": "reset"
                }
            }"##,
        );

        let theme = load_custom_theme(&path).unwrap();
        assert_eq!(theme.name(), "custom-theme");
        assert!(matches!(theme, Theme::Custom(_)));
    }

    #[test]
    fn rejects_custom_theme_names_with_whitespace() {
        let path = temp_theme_file(
            "bad.json",
            r#"{ "name": "two words", "styles": { "string": "red" } }"#,
        );

        assert!(load_custom_theme(&path).is_err());
    }
}
