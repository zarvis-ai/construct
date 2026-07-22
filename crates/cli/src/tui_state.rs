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

use construct_protocol::paths::Paths;
use serde::{Deserialize, Serialize};

fn default_hide_pane_side_borders() -> bool {
    true
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WidgetState {
    #[serde(default)]
    pub visible: Vec<String>,
}

fn default_matrix_rain_hidden() -> bool {
    true
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
    #[serde(default = "default_matrix_rain_hidden")]
    pub matrix_rain_hidden: bool,
    #[serde(default = "default_hide_pane_side_borders")]
    pub hide_pane_side_borders: bool,
    #[serde(default)]
    pub main_windows: Option<crate::app::MainWindowTree>,
    #[serde(default)]
    pub active_window_id: Option<u64>,
    #[serde(default)]
    pub open_program_session_ids: Vec<String>,
    /// Parent session ids whose subagent/fork trees were collapsed when the
    /// TUI last quit. Stale ids are pruned against the live session graph when
    /// the state is restored and saved.
    #[serde(default)]
    pub collapsed_session_ids: Vec<String>,
    #[serde(default)]
    pub widgets: HashMap<String, WidgetState>,
    /// Step (1..=8) of an interactive tutorial (spec 0077) in progress when
    /// the TUI last quit, so an interrupted tour resumes at the same step
    /// instead of restarting from step 1. `None` = no tour in progress —
    /// cleared as soon as the tour ends, whether by completion or
    /// `[end tour]`.
    #[serde(default)]
    pub tutorial_step: Option<u8>,
    /// Whether the sidebar's lineage section (spec 0081) was collapsed to
    /// just its header when the TUI last quit.
    #[serde(default)]
    pub lineage_collapsed: bool,
    /// User drag-resized height of the lineage section (header included);
    /// `None` = size to content.
    #[serde(default)]
    pub lineage_h: Option<u16>,
    /// Whether the lineage section is in the compact (rails) view. The
    /// default is the full boxed-lane diagram. Deliberately a NEW key
    /// (`lineage_compact`, not the old `lineage_view_compact`): every state
    /// blob auto-saved while compact was briefly the default carries a
    /// stale `true` that never reflected a user choice, so the old key is
    /// ignored rather than migrated.
    #[serde(default)]
    pub lineage_compact: bool,
    /// Whether the session list is in the two-line full-card view. The
    /// default is the one-line compact view.
    #[serde(default)]
    pub session_list_full: bool,
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
            matrix_rain_hidden: true,
            hide_pane_side_borders: default_hide_pane_side_borders(),
            main_windows: None,
            active_window_id: None,
            open_program_session_ids: Vec::new(),
            collapsed_session_ids: Vec::new(),
            widgets: HashMap::new(),
            tutorial_step: None,
            lineage_collapsed: false,
            lineage_h: None,
            lineage_compact: false,
            session_list_full: false,
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

/// Marker file (spec 0077) recording that the interactive tutorial has been
/// completed at least once. Only written on a genuine finish (the final
/// step's completion, or `[end tour]` clicked from that final step) — never
/// on an early `[end tour]` or a mid-tour quit, so a user who bails out
/// partway is still invited to come back and finish. A separate file from
/// `configure-seen`, following the same rationale: cheap "have we shown this"
/// check without parsing the full state blob.
fn tutorial_done_marker_path() -> PathBuf {
    Paths::discover().state_dir.join("tutorial-done")
}

pub fn tutorial_done() -> bool {
    tutorial_done_marker_path().exists()
}

pub fn mark_tutorial_done() {
    let path = tutorial_done_marker_path();
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
        assert!(state.collapsed_session_ids.is_empty());
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

    #[test]
    fn state_round_trips_collapsed_session_ids() {
        let state = TuiState {
            collapsed_session_ids: vec!["parent-1".into(), "parent-2".into()],
            ..TuiState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize");
        let restored: TuiState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.collapsed_session_ids, vec!["parent-1", "parent-2"]);
    }

    #[test]
    fn state_round_trips_tutorial_step() {
        let state = TuiState {
            tutorial_step: Some(4),
            ..TuiState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize");
        let restored: TuiState = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.tutorial_step, Some(4));
    }

    #[test]
    fn state_round_trips_lineage_collapse_and_view_mode() {
        let state = TuiState {
            lineage_collapsed: true,
            lineage_compact: true,
            ..TuiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: TuiState = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.lineage_collapsed);
        assert!(restored.lineage_compact);

        // Legacy blobs default to an expanded section in the FULL boxed
        // view — including blobs carrying the retired `lineage_view_compact`
        // key, whose auto-saved value never reflected a user choice.
        let legacy: TuiState = serde_json::from_str(
            r#"{"last_selected_session_id": "s1", "lineage_view_compact": true}"#,
        )
        .expect("legacy");
        assert!(!legacy.lineage_collapsed);
        assert!(!legacy.lineage_compact);
    }

    #[test]
    fn state_round_trips_session_list_view_mode() {
        let state = TuiState {
            session_list_full: true,
            ..TuiState::default()
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let restored: TuiState = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.session_list_full);

        // Legacy blobs default to the compact one-line view.
        let legacy: TuiState =
            serde_json::from_str(r#"{"last_selected_session_id": "s1"}"#).expect("legacy");
        assert!(!legacy.session_list_full);
    }

    #[test]
    fn legacy_state_defaults_tutorial_step_to_none() {
        let state: TuiState = serde_json::from_str(r#"{"last_selected_session_id": "s1"}"#)
            .expect("legacy state should deserialize");

        assert_eq!(state.tutorial_step, None);
    }
}
