//! User-editable TUI color theme.
//!
//! Defaults to a Matrix-inspired palette. Users can override any slot in:
//!
//! ```toml
//! # $AGENTD_CONFIG_DIR/theme.toml, default ~/.config/agentd/theme.toml
//! [colors]
//! text = "#b8ffcc"
//! accent = "#39ff88"
//! danger = "red"
//! ```

use agentd_protocol::paths::Paths;
use ratatui::style::Color;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Theme {
    pub text: Color,
    pub dim: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focused: Color,
    pub accent: Color,
    pub accent_alt: Color,
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
            text: Color::Rgb(190, 255, 205),
            dim: Color::Rgb(32, 112, 58),
            muted: Color::Rgb(76, 150, 92),
            border: Color::Rgb(24, 96, 48),
            border_focused: Color::Rgb(75, 255, 130),
            accent: Color::Rgb(57, 255, 136),
            accent_alt: Color::Rgb(92, 225, 255),
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

impl Theme {
    pub fn load_or_default() -> (Self, Option<String>) {
        let path = theme_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => {
                return (
                    Self::default(),
                    Some(format!("theme read failed ({}): {e}", path.display())),
                )
            }
        };
        match parse_theme(&text) {
            Ok(theme) => (theme, None),
            Err(e) => (
                Self::default(),
                Some(format!("theme parse failed ({}): {e}", path.display())),
            ),
        }
    }
}

pub fn theme_file() -> PathBuf {
    Paths::discover().config_dir.join("theme.toml")
}

fn parse_theme(text: &str) -> Result<Theme, String> {
    let raw: RawTheme = toml::from_str(text).map_err(|e| e.to_string())?;
    let colors = raw.colors.unwrap_or_default();
    let mut theme = Theme::default();
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

#[derive(Debug, Deserialize)]
struct RawTheme {
    #[serde(default)]
    colors: Option<RawColors>,
}

#[derive(Debug, Default, Deserialize)]
struct RawColors {
    text: Option<String>,
    dim: Option<String>,
    muted: Option<String>,
    border: Option<String>,
    border_focused: Option<String>,
    accent: Option<String>,
    accent_alt: Option<String>,
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

    #[test]
    fn parses_partial_theme_over_default() {
        let theme = parse_theme(
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
}
