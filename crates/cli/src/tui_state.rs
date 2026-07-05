//! Tiny per-user TUI preferences that persist across launches.
//!
//! Only the bare minimum lives here: what session was selected
//! last and a few layout preferences, so reopening the TUI lands
//! the user back where they left off. The file is JSON to stay
//! forgiving — extra keys from a newer client read by an older one
//! are ignored, and a corrupt file just resets to defaults instead
//! of failing the launch.

use std::collections::HashMap;
use std::path::PathBuf;

use agentd_protocol::paths::Paths;
use serde::{Deserialize, Serialize};

fn default_hide_pane_side_borders() -> bool {
    true
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WidgetState {
    #[serde(default)]
    pub visible: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiState {
    #[serde(default)]
    pub last_selected_session_id: Option<String>,
    #[serde(default)]
    pub zoom: crate::app::ZoomMode,
    #[serde(default)]
    pub list_panel_w: Option<u16>,
    #[serde(default)]
    pub pin_strip_h: Option<u16>,
    #[serde(default)]
    pub orchestrator_panel_h: Option<u16>,
    #[serde(default)]
    pub matrix_rain_h: Option<u16>,
    #[serde(default)]
    pub list_collapsed: bool,
    #[serde(default)]
    pub matrix_rain_hidden: bool,
    #[serde(default = "default_hide_pane_side_borders")]
    pub hide_pane_side_borders: bool,
    #[serde(default)]
    pub main_windows: Option<crate::app::MainWindowTree>,
    #[serde(default)]
    pub active_window_id: Option<u64>,
    #[serde(default)]
    pub open_program_session_ids: Vec<String>,
    #[serde(default)]
    pub widgets: HashMap<String, WidgetState>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            last_selected_session_id: None,
            zoom: crate::app::ZoomMode::default(),
            list_panel_w: None,
            pin_strip_h: None,
            orchestrator_panel_h: None,
            matrix_rain_h: None,
            list_collapsed: false,
            matrix_rain_hidden: false,
            hide_pane_side_borders: default_hide_pane_side_borders(),
            main_windows: None,
            active_window_id: None,
            open_program_session_ids: Vec::new(),
            widgets: HashMap::new(),
        }
    }
}

fn state_path() -> PathBuf {
    Paths::discover().tui_state_file()
}

/// Marker file (spec 0069) recording that the `/configure` dialog has been
/// dismissed at least once, so it only auto-opens unprompted on a genuinely
/// fresh install — not every launch. Deliberately a separate file from
/// `tui-state.json` (rewritten wholesale on every quit) so checking "have we
/// shown this before" doesn't require parsing the full state blob.
fn configure_seen_marker_path() -> PathBuf {
    Paths::discover().state_dir.join("configure-seen")
}

pub fn configure_dialog_seen() -> bool {
    configure_seen_marker_path().exists()
}

pub fn mark_configure_dialog_seen() {
    let path = configure_seen_marker_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, b"");
}

pub fn load() -> TuiState {
    let path = state_path();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return TuiState::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save(state: &TuiState) {
    let path = state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_state_defaults_open_program_ids() {
        let state: TuiState = serde_json::from_str(
            r#"{
                "last_selected_session_id": "s1",
                "hide_pane_side_borders": true
            }"#,
        )
        .expect("legacy state should deserialize");

        assert!(state.open_program_session_ids.is_empty());
    }

    #[test]
    fn state_round_trips_open_program_ids() {
        let state = TuiState {
            open_program_session_ids: vec!["s1".into(), "s2".into()],
            ..TuiState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize");
        let restored: TuiState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.open_program_session_ids, vec!["s1", "s2"]);
    }
}
