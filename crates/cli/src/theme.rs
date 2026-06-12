//! User-editable TUI color theme.
//!
//! Ships a Matrix-inspired palette in dark and light variants. By default
//! (`mode = "auto"`) the active variant is chosen at startup by querying the
//! terminal's background color (OSC 11); `mode = "light"`/`"dark"` force one.
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

/// Whether the TUI palette tracks the terminal's background. `Auto` queries the
/// terminal (OSC 11) and picks the light or dark base; `Light`/`Dark` force it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

/// Parsed `theme.toml` (the `mode` + raw `[colors]` text), kept so the final
/// palette can be resolved *after* the terminal background is detected.
pub struct ThemeConfig {
    pub mode: ThemeMode,
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
                    text: String::new(),
                    warning: None,
                }
            }
            Err(e) => {
                return Self {
                    mode: ThemeMode::Auto,
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
            text,
            warning,
        }
    }

    /// Build the final palette. `detected_light` is the OSC 11 result for
    /// `Auto` (ignored for forced modes); `None` falls back to dark.
    pub fn resolve(&self, detected_light: Option<bool>) -> Theme {
        let light = match self.mode {
            ThemeMode::Light => true,
            ThemeMode::Dark => false,
            ThemeMode::Auto => detected_light.unwrap_or(false),
        };
        let base = if light { Theme::light() } else { Theme::dark() };
        if self.text.is_empty() {
            return base;
        }
        // Overrides were validated at load; ignore errors here.
        parse_theme_onto(base.clone(), &self.text).unwrap_or(base)
    }
}

impl Theme {
    /// The default Matrix palette, tuned for a dark terminal background.
    pub fn dark() -> Self {
        Self::default()
    }

    /// Matrix-flavored palette tuned for a *light* terminal background:
    /// dark-green functional text + darker greens for the rain heads (so they
    /// read on white), with pale greens for the fading tails.
    pub fn light() -> Self {
        Self {
            text: Color::Rgb(20, 52, 32),
            dim: Color::Rgb(96, 140, 108),
            muted: Color::Rgb(108, 128, 114),
            border: Color::Rgb(150, 190, 162),
            border_focused: Color::Rgb(20, 140, 78),
            accent: Color::Rgb(16, 138, 74),
            accent_alt: Color::Rgb(22, 110, 150),
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

    /// Back-compat: dark palette + theme.toml color overrides, no detection.
    pub fn load_or_default() -> (Self, Option<String>) {
        let cfg = ThemeConfig::load();
        (cfg.resolve(Some(false)), cfg.warning)
    }
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

pub fn theme_file() -> PathBuf {
    Paths::discover().config_dir.join("theme.toml")
}

/// Query the terminal's background color via OSC 11 and return `Some(true)` for
/// a light background, `Some(false)` for dark, or `None` if the terminal didn't
/// answer within `timeout` (caller falls back). Must run in raw mode, before
/// the event loop starts consuming stdin.
#[cfg(unix)]
pub fn detect_terminal_is_light(timeout: std::time::Duration) -> Option<bool> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    use std::time::Instant;

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
        let n = unsafe {
            libc::read(
                fd,
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
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

#[derive(Debug, Default, Deserialize)]
struct RawTheme {
    #[serde(default)]
    mode: Option<String>,
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
            text: String::new(),
            warning: None,
        };
        assert_eq!(dark.resolve(Some(true)).text, Theme::dark().text);
        let light = ThemeConfig {
            mode: ThemeMode::Light,
            text: String::new(),
            warning: None,
        };
        assert_eq!(light.resolve(Some(false)).text, Theme::light().text);
    }

    #[test]
    fn auto_mode_follows_detection_and_falls_back_dark() {
        let auto = ThemeConfig {
            mode: ThemeMode::Auto,
            text: String::new(),
            warning: None,
        };
        assert_eq!(auto.resolve(Some(true)).text, Theme::light().text);
        assert_eq!(auto.resolve(Some(false)).text, Theme::dark().text);
        assert_eq!(auto.resolve(None).text, Theme::dark().text); // no answer → dark
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
