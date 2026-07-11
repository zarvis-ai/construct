//! User-editable TUI color theme.
//!
//! Ships a Matrix-inspired palette in dark and light variants. By default
//! (`mode = "auto"`) the active variant is chosen at startup by querying the
//! terminal's background color (OSC 11); `mode = "light"`/`"dark"` force one.
//! The runtime `/theme` command writes a named `theme = "matrix" | "basic" |
//! "dark" | "light"` choice, which takes precedence over legacy `mode`.
//! Individual slots can be overridden on top of the active variant:
//!
//! ```toml
//! # $CONSTRUCT_CONFIG_DIR/theme.toml, default ~/.config/construct/theme.toml
//! mode = "auto"   # "auto" | "light" | "dark"
//! [colors]
//! text = "#b8ffcc"
//! accent = "#39ff88"
//! danger = "red"
//! ```

use construct_protocol::paths::Paths;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Theme {
    /// Full-frame terminal background. `None` leaves the terminal's own
    /// background visible, which is intentional for background-aware themes.
    pub background: Option<Color>,
    pub text: Color,
    pub dim: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focused: Color,
    pub accent: Color,
    pub accent_alt: Color,
    /// The Program pane's frame color. Fixed to the Matrix palette's cyan
    /// (dark- or light-background variant) in every theme, so the Program
    /// frame reads as the same distinct accent no matter which theme is
    /// active — unlike `accent_alt`, which follows the active theme.
    pub program_border: Color,
    pub highlight_fg: Color,
    pub highlight_bg: Color,
    pub inactive_highlight_bg: Color,
    pub modeline_fg: Color,
    pub modeline_bg: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub group: Color,
    pub harness: Color,
    pub user: Color,
    pub assistant: Color,
    pub system: Color,
    pub tool: Color,
    pub matrix_dim: Color,
    pub matrix_line: Color,
    pub matrix_close: Color,
    pub matrix_glow: Color,
    pub matrix_flash_work: Color,
    pub matrix_flash_good: Color,
    pub matrix_flash_warn: Color,
    pub matrix_flash_bad: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: None,
            text: Color::Rgb(190, 255, 205),
            dim: Color::Rgb(32, 112, 58),
            muted: Color::Rgb(76, 150, 92),
            border: Color::Rgb(24, 96, 48),
            border_focused: Color::Rgb(75, 255, 130),
            accent: Color::Rgb(57, 255, 136),
            accent_alt: Color::Rgb(92, 225, 255),
            program_border: Color::Rgb(92, 225, 255),
            highlight_fg: Color::Rgb(6, 22, 12),
            highlight_bg: Color::Rgb(78, 255, 130),
            inactive_highlight_bg: Color::Rgb(28, 78, 42),
            modeline_fg: Color::Rgb(205, 255, 215),
            modeline_bg: Color::Rgb(8, 46, 24),
            success: Color::Rgb(125, 255, 115),
            warning: Color::Rgb(255, 215, 92),
            danger: Color::Rgb(255, 95, 90),
            info: Color::Rgb(92, 225, 255),
            group: Color::Rgb(150, 255, 170),
            harness: Color::Rgb(150, 255, 170),
            user: Color::Rgb(225, 255, 230),
            assistant: Color::Rgb(145, 255, 165),
            system: Color::Rgb(60, 140, 76),
            tool: Color::Rgb(125, 255, 115),
            matrix_dim: Color::Rgb(18, 92, 42),
            matrix_line: Color::Rgb(30, 105, 54),
            matrix_close: Color::Rgb(150, 255, 170),
            matrix_glow: Color::Rgb(52, 132, 78),
            matrix_flash_work: Color::Rgb(165, 255, 190),
            matrix_flash_good: Color::Rgb(150, 255, 120),
            matrix_flash_warn: Color::Rgb(255, 210, 90),
            matrix_flash_bad: Color::Rgb(255, 95, 90),
        }
    }
}

/// Whether the TUI palette tracks the terminal's background. `Auto` queries the
/// terminal (OSC 11) and picks the light or dark base; `Light`/`Dark` force it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

/// User-visible named UI themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    Matrix,
    Basic,
    Dark,
    Light,
}

impl ThemeName {
    pub const ALL: [ThemeName; 4] = [
        ThemeName::Matrix,
        ThemeName::Basic,
        ThemeName::Dark,
        ThemeName::Light,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "matrix" | "green" => Some(Self::Matrix),
            "basic" | "plain" | "ansi" => Some(Self::Basic),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Matrix => "matrix",
            Self::Basic => "basic",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Matrix => "matrix",
            Self::Basic => "basic",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Matrix => Self::Basic,
            Self::Basic => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Matrix,
        }
    }

    pub fn is_background_aware(self) -> bool {
        matches!(self, Self::Matrix | Self::Basic)
    }
}

/// Parsed `theme.toml` (the `mode` + raw `[colors]` text), kept so the final
/// palette can be resolved *after* the terminal background is detected.
pub struct ThemeConfig {
    pub mode: ThemeMode,
    pub name: Option<ThemeName>,
    /// Raw file contents (empty if no theme.toml), re-applied as `[colors]`
    /// overrides onto whichever base palette is chosen.
    text: String,
    pub warning: Option<String>,
}

impl ThemeConfig {
    pub fn load() -> Self {
        let path = theme_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Self {
                    mode: ThemeMode::Auto,
                    name: None,
                    text: String::new(),
                    warning: None,
                }
            }
            Err(e) => {
                return Self {
                    mode: ThemeMode::Auto,
                    name: None,
                    text: String::new(),
                    warning: Some(format!("theme read failed ({}): {e}", path.display())),
                }
            }
        };
        // Validate the color overrides up front (against the dark base) to
        // surface a single warning; the actual base is chosen later in resolve.
        let warning = parse_theme_onto(Theme::dark(), &text)
            .err()
            .map(|e| format!("theme parse failed ({}): {e}", path.display()));
        Self {
            mode: parse_mode(&text),
            name: parse_theme_name(&text),
            text,
            warning,
        }
    }

    /// Build the final palette. `detected_light` is the OSC 11 result for
    /// `Auto` (ignored for forced modes); `None` falls back to dark.
    pub fn resolve(&self, detected_light: Option<bool>) -> Theme {
        let base = match self.name {
            Some(name) => Theme::named_for_terminal(name, detected_light),
            None => {
                let light = match self.mode {
                    ThemeMode::Light => true,
                    ThemeMode::Dark => false,
                    ThemeMode::Auto => detected_light.unwrap_or(false),
                };
                if light {
                    Theme::light()
                } else {
                    Theme::dark()
                }
            }
        };
        if self.text.is_empty() {
            return base;
        }
        // Overrides were validated at load; ignore errors here.
        parse_theme_onto(base.clone(), &self.text).unwrap_or(base)
    }

    pub fn active_name(&self, detected_light: Option<bool>) -> ThemeName {
        if let Some(name) = self.name {
            return name;
        }
        match self.mode {
            ThemeMode::Light => ThemeName::Light,
            ThemeMode::Dark => ThemeName::Matrix,
            ThemeMode::Auto => {
                if detected_light.unwrap_or(false) {
                    ThemeName::Light
                } else {
                    ThemeName::Matrix
                }
            }
        }
    }

    pub fn select_named_for_terminal(
        &mut self,
        name: ThemeName,
        detected_light: Option<bool>,
    ) -> Result<Theme, String> {
        persist_named_theme(name)?;
        self.name = Some(name);
        self.text = set_theme_line(&self.text, name);
        Ok(self.resolve(detected_light))
    }
}

impl Theme {
    pub fn named_for_terminal(name: ThemeName, detected_light: Option<bool>) -> Self {
        match name {
            ThemeName::Matrix => {
                if detected_light.unwrap_or(false) {
                    Self::light()
                } else {
                    Self::dark()
                }
            }
            ThemeName::Basic => {
                if detected_light.unwrap_or(false) {
                    Self::basic_light()
                } else {
                    Self::basic_dark()
                }
            }
            ThemeName::Dark => Self::dark_ui(),
            ThemeName::Light => Self::light_ui(),
        }
    }

    /// The default Matrix palette, tuned for a dark terminal background.
    pub fn dark() -> Self {
        Self::default()
    }

    /// Matrix-flavored palette tuned for a *light* terminal background:
    /// dark-green functional text + darker greens for the rain heads (so they
    /// read on white), with pale greens for the fading tails.
    pub fn light() -> Self {
        Self {
            background: None,
            text: Color::Rgb(20, 52, 32),
            dim: Color::Rgb(96, 140, 108),
            muted: Color::Rgb(108, 128, 114),
            border: Color::Rgb(150, 190, 162),
            border_focused: Color::Rgb(20, 140, 78),
            accent: Color::Rgb(16, 138, 74),
            accent_alt: Color::Rgb(22, 110, 150),
            program_border: Color::Rgb(22, 110, 150),
            highlight_fg: Color::Rgb(248, 255, 250),
            highlight_bg: Color::Rgb(32, 158, 92),
            inactive_highlight_bg: Color::Rgb(206, 232, 214),
            modeline_fg: Color::Rgb(238, 255, 242),
            modeline_bg: Color::Rgb(22, 110, 62),
            success: Color::Rgb(24, 150, 74),
            warning: Color::Rgb(168, 118, 12),
            danger: Color::Rgb(190, 44, 40),
            info: Color::Rgb(22, 108, 150),
            group: Color::Rgb(36, 128, 80),
            harness: Color::Rgb(36, 128, 80),
            user: Color::Rgb(40, 64, 50),
            assistant: Color::Rgb(28, 120, 72),
            system: Color::Rgb(108, 128, 114),
            tool: Color::Rgb(24, 150, 74),
            matrix_dim: Color::Rgb(176, 212, 186),
            matrix_line: Color::Rgb(120, 178, 138),
            matrix_close: Color::Rgb(28, 120, 72),
            matrix_glow: Color::Rgb(70, 160, 104),
            matrix_flash_work: Color::Rgb(28, 140, 84),
            matrix_flash_good: Color::Rgb(18, 150, 70),
            matrix_flash_warn: Color::Rgb(176, 126, 18),
            matrix_flash_bad: Color::Rgb(190, 50, 44),
        }
    }

    /// Basic terminal-aware palette with common ANSI-style blues/grays instead
    /// of the Matrix green treatment. Tuned for a dark terminal background.
    pub fn basic_dark() -> Self {
        Self {
            background: None,
            text: Color::Rgb(229, 231, 235),
            dim: Color::Rgb(107, 114, 128),
            muted: Color::Rgb(156, 163, 175),
            border: Color::Rgb(64, 64, 64),
            border_focused: Color::Rgb(224, 224, 224),
            accent: Color::Rgb(96, 165, 250),
            accent_alt: Color::Rgb(192, 132, 252),
            program_border: Color::Rgb(92, 225, 255),
            highlight_fg: Color::Rgb(17, 24, 39),
            highlight_bg: Color::Rgb(147, 197, 253),
            inactive_highlight_bg: Color::Rgb(55, 65, 81),
            modeline_fg: Color::Rgb(249, 250, 251),
            modeline_bg: Color::Rgb(31, 41, 55),
            success: Color::Rgb(34, 197, 94),
            warning: Color::Rgb(234, 179, 8),
            danger: Color::Rgb(239, 68, 68),
            info: Color::Rgb(56, 189, 248),
            group: Color::Rgb(209, 213, 219),
            harness: Color::Rgb(209, 213, 219),
            user: Color::Rgb(249, 250, 251),
            assistant: Color::Rgb(191, 219, 254),
            system: Color::Rgb(156, 163, 175),
            tool: Color::Rgb(167, 139, 250),
            matrix_dim: Color::Rgb(75, 85, 99),
            matrix_line: Color::Rgb(107, 114, 128),
            matrix_close: Color::Rgb(209, 213, 219),
            matrix_glow: Color::Rgb(100, 116, 139),
            matrix_flash_work: Color::Rgb(191, 219, 254),
            matrix_flash_good: Color::Rgb(96, 165, 250),
            matrix_flash_warn: Color::Rgb(234, 179, 8),
            matrix_flash_bad: Color::Rgb(239, 68, 68),
        }
    }

    /// Basic terminal-aware palette tuned for a light terminal background.
    pub fn basic_light() -> Self {
        Self {
            background: None,
            text: Color::Rgb(31, 41, 55),
            dim: Color::Rgb(107, 114, 128),
            muted: Color::Rgb(75, 85, 99),
            border: Color::Rgb(165, 165, 165),
            border_focused: Color::Rgb(64, 64, 64),
            accent: Color::Rgb(37, 99, 235),
            accent_alt: Color::Rgb(124, 58, 237),
            program_border: Color::Rgb(22, 110, 150),
            highlight_fg: Color::Rgb(255, 255, 255),
            highlight_bg: Color::Rgb(37, 99, 235),
            inactive_highlight_bg: Color::Rgb(229, 231, 235),
            modeline_fg: Color::Rgb(255, 255, 255),
            modeline_bg: Color::Rgb(55, 65, 81),
            success: Color::Rgb(22, 163, 74),
            warning: Color::Rgb(202, 138, 4),
            danger: Color::Rgb(220, 38, 38),
            info: Color::Rgb(2, 132, 199),
            group: Color::Rgb(55, 65, 81),
            harness: Color::Rgb(55, 65, 81),
            user: Color::Rgb(17, 24, 39),
            assistant: Color::Rgb(29, 78, 216),
            system: Color::Rgb(107, 114, 128),
            tool: Color::Rgb(124, 58, 237),
            matrix_dim: Color::Rgb(209, 213, 219),
            matrix_line: Color::Rgb(156, 163, 175),
            matrix_close: Color::Rgb(55, 65, 81),
            matrix_glow: Color::Rgb(148, 163, 184),
            matrix_flash_work: Color::Rgb(29, 78, 216),
            matrix_flash_good: Color::Rgb(37, 99, 235),
            matrix_flash_warn: Color::Rgb(202, 138, 4),
            matrix_flash_bad: Color::Rgb(220, 38, 38),
        }
    }

    /// Neutral dark palette for users who want Construct without the Matrix hue.
    pub fn dark_ui() -> Self {
        Self {
            background: Some(Color::Rgb(12, 18, 27)),
            text: Color::Rgb(232, 236, 243),
            dim: Color::Rgb(102, 112, 128),
            muted: Color::Rgb(145, 153, 166),
            border: Color::Rgb(64, 64, 64),
            border_focused: Color::Rgb(224, 224, 224),
            accent: Color::Rgb(121, 184, 255),
            accent_alt: Color::Rgb(255, 176, 84),
            program_border: Color::Rgb(92, 225, 255),
            highlight_fg: Color::Rgb(12, 18, 28),
            highlight_bg: Color::Rgb(121, 184, 255),
            inactive_highlight_bg: Color::Rgb(43, 52, 66),
            modeline_fg: Color::Rgb(232, 236, 243),
            modeline_bg: Color::Rgb(29, 38, 52),
            success: Color::Rgb(99, 201, 127),
            warning: Color::Rgb(238, 190, 90),
            danger: Color::Rgb(244, 107, 116),
            info: Color::Rgb(121, 184, 255),
            group: Color::Rgb(176, 191, 212),
            harness: Color::Rgb(176, 191, 212),
            user: Color::Rgb(245, 247, 250),
            assistant: Color::Rgb(183, 210, 245),
            system: Color::Rgb(126, 136, 150),
            tool: Color::Rgb(99, 201, 127),
            matrix_dim: Color::Rgb(51, 62, 78),
            matrix_line: Color::Rgb(76, 92, 112),
            matrix_close: Color::Rgb(176, 191, 212),
            matrix_glow: Color::Rgb(91, 112, 138),
            matrix_flash_work: Color::Rgb(183, 210, 245),
            matrix_flash_good: Color::Rgb(99, 201, 127),
            matrix_flash_warn: Color::Rgb(238, 190, 90),
            matrix_flash_bad: Color::Rgb(244, 107, 116),
        }
    }

    /// Neutral light palette with dark text and restrained color accents.
    pub fn light_ui() -> Self {
        Self {
            background: Some(Color::Rgb(246, 248, 251)),
            text: Color::Rgb(34, 40, 49),
            dim: Color::Rgb(125, 135, 148),
            muted: Color::Rgb(92, 103, 118),
            border: Color::Rgb(165, 165, 165),
            border_focused: Color::Rgb(64, 64, 64),
            accent: Color::Rgb(34, 115, 195),
            accent_alt: Color::Rgb(174, 96, 28),
            program_border: Color::Rgb(22, 110, 150),
            highlight_fg: Color::Rgb(255, 255, 255),
            highlight_bg: Color::Rgb(34, 115, 195),
            inactive_highlight_bg: Color::Rgb(226, 232, 240),
            modeline_fg: Color::Rgb(255, 255, 255),
            modeline_bg: Color::Rgb(52, 86, 128),
            success: Color::Rgb(37, 135, 72),
            warning: Color::Rgb(172, 113, 24),
            danger: Color::Rgb(190, 56, 68),
            info: Color::Rgb(34, 115, 195),
            group: Color::Rgb(64, 82, 104),
            harness: Color::Rgb(64, 82, 104),
            user: Color::Rgb(20, 24, 32),
            assistant: Color::Rgb(42, 88, 145),
            system: Color::Rgb(112, 122, 136),
            tool: Color::Rgb(37, 135, 72),
            matrix_dim: Color::Rgb(218, 225, 234),
            matrix_line: Color::Rgb(184, 196, 210),
            matrix_close: Color::Rgb(64, 82, 104),
            matrix_glow: Color::Rgb(146, 164, 184),
            matrix_flash_work: Color::Rgb(42, 88, 145),
            matrix_flash_good: Color::Rgb(37, 135, 72),
            matrix_flash_warn: Color::Rgb(172, 113, 24),
            matrix_flash_bad: Color::Rgb(190, 56, 68),
        }
    }

    /// Back-compat: dark palette + theme.toml color overrides, no detection.
    pub fn load_or_default() -> (Self, Option<String>) {
        let cfg = ThemeConfig::load();
        (cfg.resolve(Some(false)), cfg.warning)
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn border_style(&self) -> Style {
        Style::default().fg(self.border)
    }

    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn text_style(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn themed_block(&self, title: impl Into<String>) -> Block<'static> {
        Block::default()
            .borders(Borders::ALL)
            .border_style(self.border_style())
            .title(Line::from(title.into()))
    }

    /// The frame background this theme paints, as 8-bit RGB, reported to the
    /// daemon so it can answer child OSC 11 background probes (spec 0073).
    /// Matrix/basic return `None` so the outer terminal remains the authority.
    pub fn background_rgb(&self) -> Option<[u8; 3]> {
        let (r, g, b) = color_to_rgb(self.background?)?;
        Some([r, g, b])
    }
}

fn color_to_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset => None,
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((128, 0, 0)),
        Color::Green => Some((0, 128, 0)),
        Color::Yellow => Some((128, 128, 0)),
        Color::Blue => Some((0, 0, 128)),
        Color::Magenta => Some((128, 0, 128)),
        Color::Cyan => Some((0, 128, 128)),
        Color::Gray => Some((192, 192, 192)),
        Color::DarkGray => Some((128, 128, 128)),
        Color::LightRed => Some((255, 0, 0)),
        Color::LightGreen => Some((0, 255, 0)),
        Color::LightYellow => Some((255, 255, 0)),
        Color::LightBlue => Some((0, 0, 255)),
        Color::LightMagenta => Some((255, 0, 255)),
        Color::LightCyan => Some((0, 255, 255)),
        Color::White => Some((255, 255, 255)),
        Color::Indexed(idx) => Some(indexed_color_to_rgb(idx)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
    }
}

fn indexed_color_to_rgb(idx: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if idx < 16 {
        return ANSI16[idx as usize];
    }
    if idx < 232 {
        let n = idx - 16;
        let component = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        return (component(n / 36), component((n / 6) % 6), component(n % 6));
    }
    let gray = 8 + (idx - 232) * 10;
    (gray, gray, gray)
}

fn parse_mode(text: &str) -> ThemeMode {
    match toml::from_str::<RawTheme>(text)
        .ok()
        .and_then(|r| r.mode)
        .as_deref()
    {
        Some("light") => ThemeMode::Light,
        Some("dark") => ThemeMode::Dark,
        _ => ThemeMode::Auto,
    }
}

fn parse_theme_name(text: &str) -> Option<ThemeName> {
    toml::from_str::<RawTheme>(text)
        .ok()
        .and_then(|r| r.theme)
        .and_then(|name| ThemeName::parse(&name))
}

pub fn theme_file() -> PathBuf {
    Paths::discover().config_dir.join("theme.toml")
}

fn persist_named_theme(name: ThemeName) -> Result<(), String> {
    let path = theme_file();
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("theme read failed ({}): {e}", path.display())),
    };
    let updated = set_theme_line(&existing, name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("theme dir create failed ({}): {e}", parent.display()))?;
    }
    fs::write(&path, updated).map_err(|e| format!("theme write failed ({}): {e}", path.display()))
}

fn set_theme_line(text: &str, name: ThemeName) -> String {
    let mut out = Vec::new();
    let mut in_top_level = true;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            in_top_level = false;
        }
        if in_top_level
            && trimmed
                .strip_prefix("theme")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            continue;
        }
        out.push(line);
    }
    let mut updated = format!("theme = \"{}\"\n", name.as_str());
    if !out.is_empty() {
        updated.push_str(&out.join("\n"));
        updated.push('\n');
    }
    updated
}

/// Query the terminal's background color via OSC 11 and return `Some(true)` for
/// a light background, `Some(false)` for dark, or `None` if the terminal didn't
/// answer within `timeout` (caller falls back). Must run in raw mode, before
/// the event loop starts consuming stdin.
///
/// Do not issue the query over SSH. A terminal reply crosses the network back
/// to this process, so it can arrive after the bounded synchronous reader has
/// timed out and the crossterm event loop has taken ownership of stdin. At that
/// point crossterm decodes the OSC bytes as ordinary key events and may forward
/// them into the selected child PTY, corrupting its terminal state until its
/// next repaint. Background-aware themes deliberately use their dark fallback
/// remotely instead; users who need a fixed light palette can select `light`.
#[cfg(unix)]
pub fn detect_terminal_is_light(timeout: std::time::Duration) -> Option<bool> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::time::Instant;

    if crate::app::is_remote_session() {
        return None;
    }

    {
        let mut out = std::io::stdout();
        out.write_all(b"\x1b]11;?\x07").ok()?;
        out.flush().ok()?;
    }
    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::with_capacity(64);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let ms = remaining.as_millis().min(60_000) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, ms) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 64];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut libc::c_void, chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
        // OSC response ends with BEL (0x07) or ST (ESC \).
        if buf.contains(&0x07) || buf.windows(2).any(|w| w == [0x1b, b'\\']) {
            break;
        }
    }
    parse_osc11_luminance(&buf).map(|lum| lum > 0.5)
}

#[cfg(not(unix))]
pub fn detect_terminal_is_light(_timeout: std::time::Duration) -> Option<bool> {
    None
}

/// Parse an OSC 11 reply (`...rgb:RRRR/GGGG/BBBB...`) into perceived luminance
/// (0.0 dark .. 1.0 light). Channels may be 1–4 hex digits each.
fn parse_osc11_luminance(bytes: &[u8]) -> Option<f32> {
    let s = String::from_utf8_lossy(bytes);
    let rest = &s[s.find("rgb:")? + 4..];
    let mut parts = rest.split('/');
    let r = parse_hex_channel(parts.next()?)?;
    let g = parse_hex_channel(parts.next()?)?;
    let b = parse_hex_channel(parts.next()?)?;
    Some((0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0)
}

fn parse_hex_channel(s: &str) -> Option<u8> {
    let hex: String = s.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    if hex.is_empty() {
        return None;
    }
    let val = u32::from_str_radix(&hex, 16).ok()?;
    let max = (1u32 << (hex.len() * 4)) - 1;
    Some(((val * 255) / max) as u8)
}

fn parse_theme_onto(base: Theme, text: &str) -> Result<Theme, String> {
    let raw: RawTheme = toml::from_str(text).map_err(|e| e.to_string())?;
    let colors = raw.colors.unwrap_or_default();
    let mut theme = base;
    apply_option(&mut theme.background, colors.background, "background")?;
    apply(&mut theme.text, colors.text, "text")?;
    apply(&mut theme.dim, colors.dim, "dim")?;
    apply(&mut theme.muted, colors.muted, "muted")?;
    apply(&mut theme.border, colors.border, "border")?;
    apply(
        &mut theme.border_focused,
        colors.border_focused,
        "border_focused",
    )?;
    apply(&mut theme.accent, colors.accent, "accent")?;
    apply(&mut theme.accent_alt, colors.accent_alt, "accent_alt")?;
    apply(
        &mut theme.program_border,
        colors.program_border,
        "program_border",
    )?;
    apply(&mut theme.highlight_fg, colors.highlight_fg, "highlight_fg")?;
    apply(&mut theme.highlight_bg, colors.highlight_bg, "highlight_bg")?;
    apply(
        &mut theme.inactive_highlight_bg,
        colors.inactive_highlight_bg,
        "inactive_highlight_bg",
    )?;
    apply(&mut theme.modeline_fg, colors.modeline_fg, "modeline_fg")?;
    apply(&mut theme.modeline_bg, colors.modeline_bg, "modeline_bg")?;
    apply(&mut theme.success, colors.success, "success")?;
    apply(&mut theme.warning, colors.warning, "warning")?;
    apply(&mut theme.danger, colors.danger, "danger")?;
    apply(&mut theme.info, colors.info, "info")?;
    apply(&mut theme.group, colors.group, "group")?;
    apply(&mut theme.harness, colors.harness, "harness")?;
    apply(&mut theme.user, colors.user, "user")?;
    apply(&mut theme.assistant, colors.assistant, "assistant")?;
    apply(&mut theme.system, colors.system, "system")?;
    apply(&mut theme.tool, colors.tool, "tool")?;
    apply(&mut theme.matrix_dim, colors.matrix_dim, "matrix_dim")?;
    apply(&mut theme.matrix_line, colors.matrix_line, "matrix_line")?;
    apply(&mut theme.matrix_close, colors.matrix_close, "matrix_close")?;
    apply(&mut theme.matrix_glow, colors.matrix_glow, "matrix_glow")?;
    apply(
        &mut theme.matrix_flash_work,
        colors.matrix_flash_work,
        "matrix_flash_work",
    )?;
    apply(
        &mut theme.matrix_flash_good,
        colors.matrix_flash_good,
        "matrix_flash_good",
    )?;
    apply(
        &mut theme.matrix_flash_warn,
        colors.matrix_flash_warn,
        "matrix_flash_warn",
    )?;
    apply(
        &mut theme.matrix_flash_bad,
        colors.matrix_flash_bad,
        "matrix_flash_bad",
    )?;
    Ok(theme)
}

fn apply(slot: &mut Color, value: Option<String>, name: &str) -> Result<(), String> {
    if let Some(value) = value {
        *slot = parse_color(&value).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

fn apply_option(slot: &mut Option<Color>, value: Option<String>, name: &str) -> Result<(), String> {
    if let Some(value) = value {
        let trimmed = value.trim();
        *slot = if trimmed.eq_ignore_ascii_case("none") || trimmed.eq_ignore_ascii_case("reset") {
            None
        } else {
            Some(parse_color(trimmed).map_err(|e| format!("{name}: {e}"))?)
        };
    }
    Ok(())
}

fn parse_color(s: &str) -> Result<Color, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() != 6 {
            return Err("hex color must be #rrggbb".to_string());
        }
        let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| "bad red channel")?;
        let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| "bad green channel")?;
        let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| "bad blue channel")?;
        return Ok(Color::Rgb(r, g, b));
    }
    if let Some(idx) = s.strip_prefix("indexed:") {
        let idx = idx
            .trim()
            .parse::<u8>()
            .map_err(|_| "indexed color must be 0..255")?;
        return Ok(Color::Indexed(idx));
    }
    match s.to_ascii_lowercase().as_str() {
        "black" => Ok(Color::Black),
        "red" => Ok(Color::Red),
        "green" => Ok(Color::Green),
        "yellow" => Ok(Color::Yellow),
        "blue" => Ok(Color::Blue),
        "magenta" => Ok(Color::Magenta),
        "cyan" => Ok(Color::Cyan),
        "gray" | "grey" => Ok(Color::Gray),
        "darkgray" | "dark_gray" | "dark-grey" => Ok(Color::DarkGray),
        "lightred" | "light_red" => Ok(Color::LightRed),
        "lightgreen" | "light_green" => Ok(Color::LightGreen),
        "lightyellow" | "light_yellow" => Ok(Color::LightYellow),
        "lightblue" | "light_blue" => Ok(Color::LightBlue),
        "lightmagenta" | "light_magenta" => Ok(Color::LightMagenta),
        "lightcyan" | "light_cyan" => Ok(Color::LightCyan),
        "white" => Ok(Color::White),
        "reset" => Ok(Color::Reset),
        _ => Err(format!(
            "unknown color {s:?}; use #rrggbb, indexed:N, or a named ANSI color"
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawTheme {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    colors: Option<RawColors>,
}

#[derive(Debug, Default, Deserialize)]
struct RawColors {
    background: Option<String>,
    text: Option<String>,
    dim: Option<String>,
    muted: Option<String>,
    border: Option<String>,
    border_focused: Option<String>,
    accent: Option<String>,
    accent_alt: Option<String>,
    program_border: Option<String>,
    highlight_fg: Option<String>,
    highlight_bg: Option<String>,
    inactive_highlight_bg: Option<String>,
    modeline_fg: Option<String>,
    modeline_bg: Option<String>,
    success: Option<String>,
    warning: Option<String>,
    danger: Option<String>,
    info: Option<String>,
    group: Option<String>,
    harness: Option<String>,
    user: Option<String>,
    assistant: Option<String>,
    system: Option<String>,
    tool: Option<String>,
    matrix_dim: Option<String>,
    matrix_line: Option<String>,
    matrix_close: Option<String>,
    matrix_glow: Option<String>,
    matrix_flash_work: Option<String>,
    matrix_flash_good: Option<String>,
    matrix_flash_warn: Option<String>,
    matrix_flash_bad: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_color() {
        assert_eq!(parse_color("#39ff88"), Ok(Color::Rgb(57, 255, 136)));
    }

    /// The Program pane's frame must read as the same accent no matter which
    /// named theme is active (spec 0083) — unlike `accent_alt`, which is
    /// themed and differs across palettes (cyan/purple/orange here).
    #[test]
    fn program_border_is_fixed_to_matrix_accent_across_themes() {
        let dark_themes = [Theme::dark(), Theme::basic_dark(), Theme::dark_ui()];
        for theme in &dark_themes {
            assert_eq!(theme.program_border, Theme::dark().accent_alt);
        }
        let light_themes = [Theme::light(), Theme::basic_light(), Theme::light_ui()];
        for theme in &light_themes {
            assert_eq!(theme.program_border, Theme::light().accent_alt);
        }
        // Sanity: these themes really do disagree on `accent_alt`, so this
        // test would fail if `program_border` silently aliased it again.
        assert_ne!(Theme::basic_dark().accent_alt, Theme::dark().accent_alt);
        assert_ne!(Theme::dark_ui().accent_alt, Theme::dark().accent_alt);
    }

    /// RGB (0..=255 per channel) to HSL, returned as (hue degrees 0..360,
    /// saturation 0..1, lightness 0..1). Test-only: rendering never computes
    /// this, it just selects between pre-authored `Color::Rgb` constants
    /// (see `pane_border_style` in ui.rs).
    fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
        let r = r as f32 / 255.0;
        let g = g as f32 / 255.0;
        let b = b as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let l = (max + min) / 2.0;
        let delta = max - min;
        if delta == 0.0 {
            return (0.0, 0.0, l);
        }
        let s = delta / (1.0 - (2.0 * l - 1.0).abs());
        let h = if max == r {
            60.0 * ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            60.0 * (((b - r) / delta) + 2.0)
        } else {
            60.0 * (((r - g) / delta) + 4.0)
        };
        (h, s, l)
    }

    fn border_hsl(color: Color) -> (f32, f32, f32) {
        match color {
            Color::Rgb(r, g, b) => rgb_to_hsl(r, g, b),
            other => panic!("expected a Color::Rgb border constant, got {other:?}"),
        }
    }

    fn hue_distance(a: f32, b: f32) -> f32 {
        let d = (a - b).rem_euclid(360.0);
        d.min(360.0 - d)
    }

    /// Matrix's session border keeps a fixed green hue across focus states —
    /// the focus transition is a brightness change, never a hue change
    /// (spec 0084).
    #[test]
    fn matrix_session_border_focus_stays_same_hue() {
        for theme in [Theme::dark(), Theme::light()] {
            let (h_unfocused, _, _) = border_hsl(theme.border);
            let (h_focused, _, _) = border_hsl(theme.border_focused);
            let hue_gap = hue_distance(h_unfocused, h_focused);
            assert!(
                hue_gap <= 25.0,
                "focused border hue drifted {hue_gap} degrees from unfocused \
                 (border={:?}, border_focused={:?})",
                theme.border,
                theme.border_focused,
            );
        }
    }

    /// Basic/Dark UI/Light UI's session border is neutral grey (no hue at
    /// all) rather than a dim/vivid version of the theme's blue accent, so it
    /// reads as clearly distinct from the Program pane's fixed cyan frame
    /// (spec 0083) and can never collide with it on hue. Focus is signaled by
    /// lightness alone: dark-background themes get brighter on focus,
    /// light-background themes get darker (spec 0084).
    #[test]
    fn non_matrix_themes_session_border_is_achromatic_and_distinct_from_program_accent() {
        let themes = [
            Theme::basic_dark(),
            Theme::basic_light(),
            Theme::dark_ui(),
            Theme::light_ui(),
        ];
        for theme in &themes {
            for color in [theme.border, theme.border_focused] {
                let (_, s, _) = border_hsl(color);
                assert_eq!(s, 0.0, "expected an achromatic border, got {color:?}");
            }
            let (_, program_s, _) = border_hsl(theme.program_border);
            assert!(
                program_s > 0.0,
                "program_border should be chromatic so a zero-saturation \
                 session border can never collide with it on hue"
            );
        }
    }

    #[test]
    fn parses_partial_theme_over_default() {
        let theme = parse_theme_onto(
            Theme::dark(),
            r##"
            [colors]
            text = "#ffffff"
            accent = "cyan"
            matrix_dim = "indexed:34"
            "##,
        )
        .unwrap();
        assert_eq!(theme.text, Color::Rgb(255, 255, 255));
        assert_eq!(theme.accent, Color::Cyan);
        assert_eq!(theme.matrix_dim, Color::Indexed(34));
        assert_eq!(theme.danger, Theme::default().danger);
    }

    #[test]
    fn default_matrix_theme_uses_green_for_tools() {
        assert_eq!(Theme::default().tool, Theme::default().success);
    }

    #[test]
    fn theme_name_parses_and_cycles_basic() {
        assert_eq!(ThemeName::parse("basic"), Some(ThemeName::Basic));
        assert_eq!(ThemeName::parse("plain"), Some(ThemeName::Basic));
        assert_eq!(ThemeName::Matrix.next(), ThemeName::Basic);
        assert_eq!(ThemeName::Basic.next(), ThemeName::Dark);
    }

    #[test]
    fn theme_mode_parses_from_config() {
        assert_eq!(parse_mode(""), ThemeMode::Auto);
        assert_eq!(parse_mode("mode = \"auto\""), ThemeMode::Auto);
        assert_eq!(parse_mode("mode = \"light\""), ThemeMode::Light);
        assert_eq!(parse_mode("mode = \"dark\""), ThemeMode::Dark);
        assert_eq!(parse_mode("mode = \"weird\""), ThemeMode::Auto);
    }

    #[test]
    fn forced_mode_ignores_detection() {
        // Dark mode stays dark even if the terminal reports light, and vice versa.
        let dark = ThemeConfig {
            mode: ThemeMode::Dark,
            name: None,
            text: String::new(),
            warning: None,
        };
        assert_eq!(dark.resolve(Some(true)).text, Theme::dark().text);
        let light = ThemeConfig {
            mode: ThemeMode::Light,
            name: None,
            text: String::new(),
            warning: None,
        };
        assert_eq!(light.resolve(Some(false)).text, Theme::light().text);
    }

    #[test]
    fn auto_mode_follows_detection_and_falls_back_dark() {
        let auto = ThemeConfig {
            mode: ThemeMode::Auto,
            name: None,
            text: String::new(),
            warning: None,
        };
        assert_eq!(auto.resolve(Some(true)).text, Theme::light().text);
        assert_eq!(auto.resolve(Some(false)).text, Theme::dark().text);
        assert_eq!(auto.resolve(None).text, Theme::dark().text); // no answer → dark
    }

    #[test]
    fn named_theme_takes_precedence_over_legacy_mode() {
        let cfg = ThemeConfig {
            mode: ThemeMode::Light,
            name: Some(ThemeName::Dark),
            text: String::new(),
            warning: None,
        };
        assert_eq!(cfg.resolve(Some(true)).text, Theme::dark_ui().text);
    }

    #[test]
    fn named_background_aware_themes_follow_terminal_detection() {
        let cfg = ThemeConfig {
            mode: ThemeMode::Dark,
            name: Some(ThemeName::Basic),
            text: String::new(),
            warning: None,
        };
        assert_eq!(cfg.resolve(Some(false)).text, Theme::basic_dark().text);
        assert_eq!(cfg.resolve(Some(true)).text, Theme::basic_light().text);
    }

    #[test]
    fn matrix_and_basic_are_background_aware_but_dark_light_paint_backgrounds() {
        assert_eq!(
            Theme::named_for_terminal(ThemeName::Matrix, None).background,
            None
        );
        assert_eq!(
            Theme::named_for_terminal(ThemeName::Basic, None).background,
            None
        );
        assert_eq!(
            Theme::named_for_terminal(ThemeName::Dark, None).background,
            Some(Color::Rgb(12, 18, 27))
        );
        assert_eq!(
            Theme::named_for_terminal(ThemeName::Light, None).background,
            Some(Color::Rgb(246, 248, 251))
        );
    }

    #[test]
    fn background_override_can_force_or_clear_frame_paint() {
        let forced = parse_theme_onto(
            Theme::dark(),
            r##"
            [colors]
            background = "#101820"
            "##,
        )
        .unwrap();
        assert_eq!(forced.background, Some(Color::Rgb(16, 24, 32)));

        let cleared = parse_theme_onto(
            Theme::dark_ui(),
            r#"
            [colors]
            background = "none"
            "#,
        )
        .unwrap();
        assert_eq!(cleared.background, None);
    }

    #[test]
    fn background_rgb_reports_painted_theme_background() {
        assert_eq!(Theme::dark_ui().background_rgb(), Some([0x0c, 0x12, 0x1b]));
        assert_eq!(Theme::light_ui().background_rgb(), Some([0xf6, 0xf8, 0xfb]));
        assert_eq!(Theme::dark().background_rgb(), None);
        assert_eq!(Theme::basic_dark().background_rgb(), None);
    }

    #[test]
    fn set_theme_line_replaces_only_top_level_theme() {
        let updated = set_theme_line(
            r##"
theme = "matrix"
mode = "auto"
[colors]
theme = "#ffffff"
"##,
            ThemeName::Light,
        );
        assert!(updated.starts_with("theme = \"light\""));
        assert!(updated.contains("mode = \"auto\""));
        assert!(updated.contains("[colors]\ntheme = \"#ffffff\""));
    }

    #[test]
    fn osc11_luminance_distinguishes_light_and_dark() {
        // White background → high luminance (light).
        let white = b"\x1b]11;rgb:ffff/ffff/ffff\x07";
        assert!(parse_osc11_luminance(white).unwrap() > 0.5);
        // Black background → low luminance (dark).
        let black = b"\x1b]11;rgb:0000/0000/0000\x07";
        assert!(parse_osc11_luminance(black).unwrap() < 0.5);
        // 8-bit channels work too.
        let white8 = b"\x1b]11;rgb:ff/ff/ff\x1b\\";
        assert!(parse_osc11_luminance(white8).unwrap() > 0.5);
        // Garbage → None.
        assert!(parse_osc11_luminance(b"nope").is_none());
    }
}
