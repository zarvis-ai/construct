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

## Built-in harness child command overrides

Built-in adapters spawn their underlying CLI directly (no shell). For a
binary-only override, set the existing `AGENTD_*_BIN` env var in
`config.toml`:

```toml
[adapters.codex]
env = { AGENTD_CODEX_BIN = "/opt/homebrew/bin/codex" }
```

When the command needs a prefix or extra executable before the real CLI, use
`AGENTD_*_CMD` instead. It is split shell-style for whitespace, quotes, and
backslashes, but is still executed directly without shell expansion:

```toml
[adapters.codex]
env = { AGENTD_CODEX_CMD = "exec codex" }
```

`AGENTD_*_CMD` wins over `AGENTD_*_BIN`. Supported names:

| Harness | Full command override | Binary-only fallback |
|---|---|---|
| `codex` | `AGENTD_CODEX_CMD` | `AGENTD_CODEX_BIN` |
| `claude` | `AGENTD_CLAUDE_CMD` | `AGENTD_CLAUDE_BIN` |
| `antigravity` | `AGENTD_ANTIGRAVITY_CMD` | `AGENTD_ANTIGRAVITY_BIN` |
| `shell` | `AGENTD_SHELL_CMD` | `AGENTD_SHELL_BIN` |

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
