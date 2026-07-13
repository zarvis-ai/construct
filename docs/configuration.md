# Configuration

## Paths

`construct` reads/writes under XDG-style directories, with `CONSTRUCT_HOME` or `CONSTRUCT_*_DIR` overrides.

If `CONSTRUCT_HOME` is set, all other directories default to paths under it, i.e. `$CONSTRUCT_HOME/config`, `$CONSTRUCT_HOME/state`, `$CONSTRUCT_HOME/data`, and `$CONSTRUCT_HOME/run` respectively. Specific `CONSTRUCT_*_DIR` env variables override these defaults.

| Use | Default (without `CONSTRUCT_HOME`) | Override | Default (with `CONSTRUCT_HOME`) |
|---|---|---|---|
| Config | `~/.config/construct` | `CONSTRUCT_CONFIG_DIR` | `$CONSTRUCT_HOME/config` |
| State (pid/log) | `~/.local/state/construct` | `CONSTRUCT_STATE_DIR` | `$CONSTRUCT_HOME/state` |
| Data (sessions, projects, memory) | `~/.local/share/construct` | `CONSTRUCT_DATA_DIR` | `$CONSTRUCT_HOME/data` |
| Socket | `$XDG_RUNTIME_DIR/construct/construct.sock` (falls back to state) | `CONSTRUCT_RUNTIME_DIR` | `$CONSTRUCT_HOME/run` |

`construct paths` prints the resolved layout.

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

## Upgrade Checks

Interactive client commands check GitHub for newer releases before running. If
one is available, `construct` asks whether to upgrade now; accepting upgrades
the installed binary, restarts a running daemon, and resumes the original
command under the new binary. Daemon, adapter, ACP stdio, and other internal
invocations skip this prompt, as do non-interactive runs.

Set `CONSTRUCT_NO_UPDATE_CHECK=1` to disable both the interactive prompt and
the TUI's cached update-available notice.

## Local web UI

The daemon always serves a browser UI on localhost (no auth — this is local-only;
the public, token-protected path is `/remote-control`, see
[remote-control.md](remote-control.md)).

| Use | Default | Override |
|---|---|---|
| Web UI port | `5746` (binds `http://127.0.0.1:5746/`) | `CONSTRUCT_WEBUI_PORT` |

`construct paths` prints the resolved URL on the `webui:` line, so
you don't have to dig it out of the daemon log:

```text
$ construct paths
config:  ~/.config/construct
state:   ~/.local/state/construct
data:    ~/.local/share/construct
runtime: ~/.local/state/construct
socket:  ~/.local/state/construct/construct.sock
webui:   http://127.0.0.1:5746/
```

## Built-in harness child command overrides

Built-in adapters spawn their underlying CLI directly (no shell). For a
binary-only override, set the existing `CONSTRUCT_*_BIN` env var in
`config.toml`:

```toml
[adapters.codex]
env = { CONSTRUCT_CODEX_BIN = "/opt/homebrew/bin/codex" }
```

When the command needs a prefix or extra executable before the real CLI, use
`CONSTRUCT_*_CMD` instead. It is split shell-style for whitespace, quotes, and
backslashes, but is still executed directly without shell expansion:

```toml
[adapters.codex]
env = { CONSTRUCT_CODEX_CMD = "exec codex" }
```

`CONSTRUCT_*_CMD` wins over `CONSTRUCT_*_BIN`. Supported names:

| Harness | Full command override | Binary-only fallback |
|---|---|---|
| `codex` | `CONSTRUCT_CODEX_CMD` | `CONSTRUCT_CODEX_BIN` |
| `opencode` | `CONSTRUCT_OPENCODE_CMD` | `CONSTRUCT_OPENCODE_BIN` |
| `claude` | `CONSTRUCT_CLAUDE_CMD` | `CONSTRUCT_CLAUDE_BIN` |
| `antigravity` | `CONSTRUCT_ANTIGRAVITY_CMD` | `CONSTRUCT_ANTIGRAVITY_BIN` |
| `grok` | `CONSTRUCT_GROK_CMD` | `CONSTRUCT_GROK_BIN` |
| `shell` | `CONSTRUCT_SHELL_CMD` | `CONSTRUCT_SHELL_BIN` |

## TUI Theme

The TUI ships a Matrix-inspired palette in two variants — one for dark
terminals and one for light — and, by default, **detects which your terminal
uses** (via an OSC 11 background-color query) and picks the matching variant.
Set this in `$CONSTRUCT_CONFIG_DIR/theme.toml` (default
`~/.config/construct/theme.toml`):

```toml
mode = "auto"   # "auto" (default) | "light" | "dark"
```

- `auto` — query the terminal at startup; light background → light palette,
  dark → dark. If the terminal doesn't answer (or doesn't support the query),
  it falls back to the dark palette.
- `light` / `dark` — force a variant, skipping detection.

Override any individual color slot under `[colors]` (applied on top of whichever
variant is active):

```toml
mode = "auto"

[colors]
text = "#b8ffcc"
accent = "#39ff88"
border_focused = "#4bff82"
harness = "#96ffaa"
danger = "red"
matrix_dim = "indexed:34"
```

Colors accept `#rrggbb`, `indexed:N`, or ANSI names such as `green`, `cyan`,
`dark_gray`, and `light_yellow`. Omitted slots keep the variant's default.
