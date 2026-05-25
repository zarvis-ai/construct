# Configuration

## Paths

`agentd` reads/writes under XDG-style directories, with `AGENTD_*_DIR` overrides:

| Use | Default | Override |
|---|---|---|
| Config | `~/.config/agentd` | `AGENTD_CONFIG_DIR` |
| State (pid/log) | `~/.local/state/agentd` | `AGENTD_STATE_DIR` |
| Data (sessions, projects, memory) | `~/.local/share/agentd` | `AGENTD_DATA_DIR` |
| Socket | `$XDG_RUNTIME_DIR/agentd/agentd.sock` (falls back to state) | `AGENTD_RUNTIME_DIR` |

`agentd paths` prints the resolved layout.

The data directory stores durable, user-editable runtime data:

```text
sessions/<session-id>/
    meta.json
    transcript.jsonl
    worktree/          # optional per-session git worktree

global/
    memory.md          # cross-project memory

projects/<project-id>/
    meta.json
    memory.md          # project-specific memory
```

Legacy `groups/<project-id>.json` files are migrated to
`projects/<project-id>/meta.json` when loaded.

## TUI Theme

The TUI uses a built-in Matrix theme by default. Override any color slot in
`$AGENTD_CONFIG_DIR/theme.toml` (default `~/.config/agentd/theme.toml`):

```toml
[colors]
text = "#b8ffcc"
accent = "#39ff88"
border_focused = "#4bff82"
harness = "#96ffaa"
danger = "red"
matrix_dim = "indexed:34"
```

Colors accept `#rrggbb`, `indexed:N`, or ANSI names such as `green`, `cyan`,
`dark_gray`, and `light_yellow`. Omitted slots keep the Matrix default.
