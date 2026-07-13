//! `/configure` onboarding dialog (spec 0069): a single interactive setup
//! surface covering harness availability (tab 1) and smith's auth methods
//! (tab 2). Auto-opens on first run or when no agent harness is usable;
//! always reopenable via the command palette (`M-x` / `:` → `configure`).

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigureTab {
    Harnesses,
    SmithAuth,
}

impl ConfigureTab {
    pub fn label(self) -> &'static str {
        match self {
            ConfigureTab::Harnesses => "Harnesses",
            ConfigureTab::SmithAuth => "Smith auth",
        }
    }
}

pub const CONFIGURE_TABS: [ConfigureTab; 2] = [ConfigureTab::Harnesses, ConfigureTab::SmithAuth];

/// `App::configure_popup == None` means closed.
#[derive(Debug, Clone)]
pub struct ConfigurePopup {
    pub tab: ConfigureTab,
    pub harness_selected: usize,
    pub smith_selected: usize,
    pub smith_methods: Vec<construct_protocol::SmithAuthMethodInfo>,
    /// Which `smith_methods` entry the daemon's config currently pins, if
    /// any recognized one — see `SmithAuthStatusResult::current`.
    pub smith_current: Option<String>,
    /// Result note from the last `smith.set_auth_method` call (or an error),
    /// shown under the smith-auth diagnosis pane until the tab/selection
    /// changes or the dialog is reopened.
    pub note: Option<String>,
}

/// Client-side "how to fix" guidance for harness tab 1, keyed by harness
/// name. Deliberately not sourced from the daemon — these are static
/// instructions about the local machine (install a CLI, log in, check
/// PATH), not something the probe can express better than plain English.
pub fn harness_guidance(name: &str) -> String {
    match name {
        "claude" => "install the `claude` CLI and log in; it must be on the PATH of the shell \
                      that starts the construct daemon"
            .to_string(),
        "codex" => "install the `codex` CLI and run `codex login`; it must be on the PATH of \
                     the shell that starts the construct daemon"
            .to_string(),
        "opencode" => "install the `opencode` CLI and configure a provider with `opencode auth \
                        login`; it must be on the PATH of the shell that starts the construct \
                        daemon (or set CONSTRUCT_OPENCODE_BIN)"
            .to_string(),
        "antigravity" | "agy" => "install the `agy` CLI; it must be on the PATH of the shell that \
                           starts the construct daemon"
            .to_string(),
        "grok" => "install the `grok` CLI; it must be on the PATH of the shell that starts the \
                    construct daemon"
            .to_string(),
        "shell" => "nothing needed — always available".to_string(),
        "smith" => "see the Smith auth tab (→) for the auth methods smith supports and their \
                     live status"
            .to_string(),
        _ => "check the adapter's `binary` config in config.toml points at an installed \
              executable on the daemon's PATH"
            .to_string(),
    }
}

/// Client-side "how to obtain/set it" guidance for smith-auth tab 2, keyed
/// by `SmithAuthMethodInfo::id`. Selection there is guidance-first — there
/// is no in-dialog secret/API-key entry (spec 0069).
pub fn smith_method_guidance(id: &str) -> &'static str {
    match id {
        "anthropic_api_key" => {
            "export ANTHROPIC_API_KEY in the shell that starts the daemon, then restart the daemon"
        }
        "openai_api_key" => {
            "export OPENAI_API_KEY in the shell that starts the daemon, then restart the daemon"
        }
        "gemini_api_key" => {
            "export GEMINI_API_KEY (or GOOGLE_API_KEY) in the shell that starts the daemon, \
             then restart the daemon"
        }
        "grok_api_key" => {
            "export GROK_API_KEY (or XAI_API_KEY) in the shell that starts the daemon, then \
             restart the daemon"
        }
        "claude_subscription" => {
            "run `claude` and log in with your Claude subscription first (creates \
             ~/.claude/.credentials.json), as the user the daemon runs as, then restart the daemon"
        }
        "codex_subscription" => {
            "run `codex login` (creates ~/.codex/auth.json), then restart the daemon"
        }
        "grok_subscription" => {
            "run `grok login` (creates ~/.grok/auth.json), then restart the daemon"
        }
        "ollama" => {
            "install Ollama and run `ollama serve` so it's reachable at localhost:11434 (or \
             set OLLAMA_HOST), then restart the daemon"
        }
        "auto" => {
            "uses the first detected API key: Anthropic, then OpenAI, then Gemini; \
             subscriptions and Ollama must be picked explicitly"
        }
        _ => "",
    }
}

/// Cyclic index shift used for both tab switching and row selection —
/// `delta` steps forward/back through `count` items, wrapping either way.
fn wrap_index(current: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let current = current.min(count - 1);
    if delta < 0 {
        current
            .saturating_add(count)
            .saturating_sub(delta.unsigned_abs() % count)
            % count
    } else {
        (current + delta as usize) % count
    }
}

/// Every registered harness except `shell` reporting unavailable — condition
/// (b) for auto-opening the dialog (spec 0069): a zero-config machine with
/// no working agent harness at all.
pub fn no_agent_harness_available(harnesses: &[HarnessInfo]) -> bool {
    harnesses
        .iter()
        .filter(|h| h.name != "shell")
        .all(|h| !h.available)
}

impl App {
    /// Open the dialog, fetching fresh harness + smith-auth data. Always
    /// starts on tab 1. Marks the first-run marker immediately (on open, not
    /// on close) — a user who quits the TUI while the dialog is still open
    /// (e.g. with `C-x C-c`) must not get re-nagged on the next launch just
    /// because they never got around to dismissing it. Condition (b) of
    /// [`Self::maybe_auto_open_configure_popup`] (no agent harness available)
    /// ignores this marker entirely, so a genuinely broken setup still
    /// reopens the dialog regardless.
    pub async fn open_configure_popup(&mut self) {
        self.chord_state = ChordState::default();
        self.chord_label.clear();
        crate::tui_state::mark_configure_dialog_seen();
        self.harnesses = self.client.harnesses().await.unwrap_or_default();
        let (smith_methods, smith_current) = self.fetch_smith_auth_status().await;
        let harness_selected = self
            .configure_popup
            .as_ref()
            .map(|p| p.harness_selected)
            .unwrap_or(0);
        self.configure_popup = Some(ConfigurePopup {
            tab: ConfigureTab::Harnesses,
            harness_selected,
            smith_selected: 0,
            smith_methods,
            smith_current,
            note: None,
        });
    }

    /// Auto-open condition (spec 0069): first run (no dismiss marker yet) or
    /// no agent harness usable. Called once at startup after the initial
    /// harness probe; `self.harnesses` is already populated by then, so this
    /// doesn't need its own round trip to decide whether to open.
    pub async fn maybe_auto_open_configure_popup(&mut self) {
        let first_run = !crate::tui_state::configure_dialog_seen();
        if first_run || no_agent_harness_available(&self.harnesses) {
            self.open_configure_popup().await;
        }
    }

    async fn fetch_smith_auth_status(
        &self,
    ) -> (Vec<construct_protocol::SmithAuthMethodInfo>, Option<String>) {
        match self.client.smith_auth_status().await {
            Ok(r) => (r.methods, r.current),
            Err(_) => (Vec::new(), None),
        }
    }

    /// Periodic live refresh while the dialog is open (spec 0069), mirroring
    /// the welcome card's 5s harness poll — driven from `run_loop`.
    pub async fn refresh_configure_popup(&mut self) {
        if self.configure_popup.is_none() {
            return;
        }
        self.harnesses = self.client.harnesses().await.unwrap_or_default();
        let (methods, current) = self.fetch_smith_auth_status().await;
        if let Some(popup) = self.configure_popup.as_mut() {
            popup.smith_selected = popup.smith_selected.min(methods.len().saturating_sub(1));
            popup.smith_methods = methods;
            popup.smith_current = current;
        }
    }

    fn close_configure_popup(&mut self) {
        self.configure_popup = None;
    }

    fn configure_switch_tab_relative(&mut self, delta: isize) {
        let Some(popup) = self.configure_popup.as_mut() else {
            return;
        };
        let idx = CONFIGURE_TABS
            .iter()
            .position(|t| *t == popup.tab)
            .unwrap_or(0);
        popup.tab = CONFIGURE_TABS[wrap_index(idx, delta, CONFIGURE_TABS.len())];
    }

    fn configure_move_selection(&mut self, delta: isize) {
        let harness_count = self.harnesses.len();
        let Some(popup) = self.configure_popup.as_mut() else {
            return;
        };
        match popup.tab {
            ConfigureTab::Harnesses => {
                popup.harness_selected = wrap_index(popup.harness_selected, delta, harness_count);
            }
            ConfigureTab::SmithAuth => {
                let count = popup.smith_methods.len();
                popup.smith_selected = wrap_index(popup.smith_selected, delta, count);
            }
        }
    }

    /// Enter on the smith-auth tab: persist the highlighted method as
    /// smith's default via `smith.set_auth_method`, then show the daemon's
    /// note (always includes the "restart to take effect" caveat — spec
    /// 0069 requires the dialog never silently pretend a live pick applied
    /// to already-running adapters). A no-op on the harnesses tab, which is
    /// read-only.
    async fn confirm_configure_selection(&mut self) {
        let Some(popup) = self.configure_popup.as_ref() else {
            return;
        };
        if popup.tab != ConfigureTab::SmithAuth {
            return;
        }
        let Some(method) = popup
            .smith_methods
            .get(popup.smith_selected)
            .map(|m| m.id.clone())
        else {
            return;
        };
        match self.client.smith_set_auth_method(&method).await {
            Ok(result) => {
                if let Some(popup) = self.configure_popup.as_mut() {
                    popup.smith_current = Some(method);
                    popup.note = Some(result.note);
                }
            }
            Err(e) => {
                if let Some(popup) = self.configure_popup.as_mut() {
                    popup.note = Some(format!("failed to save: {e}"));
                }
            }
        }
    }

    /// Route a key while the dialog owns input. Esc closes; Left/Right (and
    /// Tab/BackTab) switch tabs; Up/Down move the selection; Enter confirms
    /// a smith-auth pick — these return `true` (key fully consumed, dialog
    /// stays open unless it was Esc).
    ///
    /// Anything else closes the dialog and returns `false`, telling the
    /// caller (`App::on_key`) to re-dispatch the SAME key through ordinary
    /// routing — exactly like clicking away from an open menu: the click
    /// (or keystroke) that dismisses it still takes effect. This is a
    /// general rule, not a quit-chord special case (spec 0069): `C-x x`
    /// closes the dialog and opens the command palette, `C-x C-f` closes it
    /// and opens the new-session picker, `C-x C-c` closes it and quits —
    /// none of these need their own carve-out, and a first-run dialog that
    /// happens to be on screen never again deadens a chord the user reaches
    /// for out of muscle memory.
    pub(super) async fn handle_configure_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.close_configure_popup();
                true
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.configure_switch_tab_relative(-1);
                true
            }
            KeyCode::Right | KeyCode::Tab => {
                self.configure_switch_tab_relative(1);
                true
            }
            KeyCode::Up => {
                self.configure_move_selection(-1);
                true
            }
            KeyCode::Down => {
                self.configure_move_selection(1);
                true
            }
            KeyCode::Enter => {
                self.confirm_configure_selection().await;
                true
            }
            _ => {
                self.close_configure_popup();
                false
            }
        }
    }

    /// Click on a tab header switches to it and keeps the dialog open
    /// (`true`). Any other click closes the dialog — like clicking away
    /// from an open menu — and returns `false` so the caller lets the SAME
    /// click still take effect on whatever's underneath (spec 0069),
    /// mirroring [`Self::handle_configure_key`]'s fallthrough for keys.
    pub(super) fn configure_click_tab(&mut self, col: u16, row: u16) -> bool {
        let hit = self
            .layout
            .configure_tab_hits
            .iter()
            .find(|(_, rect)| row == rect.y && col >= rect.x && col < rect.x + rect.width)
            .map(|(tab, _)| *tab);
        match hit {
            Some(tab) => {
                if let Some(popup) = self.configure_popup.as_mut() {
                    popup.tab = tab;
                }
                true
            }
            None => {
                self.close_configure_popup();
                false
            }
        }
    }
}
