# smith built-in agent

`smith` is the built-in agent that ships with construct. It talks to OpenAI,
Anthropic, Google Gemini, xAI Grok, or a local Ollama directly, and can also
delegate subscription auth to Codex, Claude Code, or the Grok CLI token store.
Smith runs its own agent loop with shell + filesystem + construct-control
tools. Many PRs for the construct repository have already been made from smith
sessions running inside construct.

### Quick start

```sh
# Pick a provider — only one of these needs to be set:
export ANTHROPIC_API_KEY=sk-ant-...
# or  export OPENAI_API_KEY=sk-...
# or  export GEMINI_API_KEY=...        # (or GOOGLE_API_KEY)
# or  export GROK_API_KEY=...          # (or XAI_API_KEY)
# or  codex login, then use --model codex-oauth:gpt-5
# or  claude login, then use --model claude-oauth:sonnet
# or  grok login, then use --model grok-oauth:grok-2-latest
# or  run a local ollama (default http://localhost:11434)

construct new smith "list the rust files in this repo and summarize what each crate does"
```

### Model selection

Pass `--model <spec>` on `construct new` (or set `CONSTRUCT_SMITH_MODEL`).
The spec is one of:

- `openai:<name>` — e.g. `openai:gpt-5-mini`
- `anthropic:<name>` — e.g. `anthropic:claude-haiku-4-5`
- `claude-oauth:<name>` — e.g. `claude-oauth:sonnet` (alias: `claude-code-oauth:`)
- `gemini:<name>` — e.g. `gemini:gemini-2.5-pro`
- `grok:<name>` — e.g. `grok:grok-2-latest` using `GROK_API_KEY` or `XAI_API_KEY`
- `grok-oauth:<name>` — e.g. `grok-oauth:grok-2-latest` using the Grok CLI auth file
- `ollama:<name>` — e.g. `ollama:llama3.1`
- `codex-oauth:<name>` — e.g. `codex-oauth:gpt-5-codex`
- `@<name>` — a named endpoint profile (see [Model profiles](#model-profiles)),
  e.g. `@deepseek` or `@deepseek:deepseek-reasoner` to override its model

Bare names auto-detect: `gpt-*` / `o[1-5]*` → OpenAI, `claude-*` →
Anthropic, `gemini-*` → Gemini, `grok*` → Grok, anything else → Ollama.
When in doubt, use the explicit prefix.

`claude-oauth:` delegates model access to the installed `claude` CLI, so run
`claude login` first and keep `claude` on `PATH` (or set `CONSTRUCT_CLAUDE_BIN`
/ `CONSTRUCT_CLAUDE_CMD`). Smith disables Claude Code's built-in tools on this
path and asks Claude for structured Smith tool calls, so construct's normal
tool approvals and transcript persistence still apply. The child CLI process
does not inherit `ANTHROPIC_API_KEY` or third-party Claude provider env vars on
this path, so the explicit `claude-oauth:` prefix does not silently become API
key billing.

`grok-oauth:` uses the same OpenAI-compatible xAI API endpoint as `grok:`, but
loads a bearer token from the Grok CLI auth file instead of `GROK_API_KEY` /
`XAI_API_KEY`. Run `grok login` first. Smith reads
`$GROK_HOME/.grok/auth.json` when `GROK_HOME` is set, otherwise
`~/.grok/auth.json`, and chooses the newest unexpired `key` entry.

If you don't pass a model and `CONSTRUCT_SMITH_MODEL` isn't set, smith
picks: `ANTHROPIC_API_KEY` → `claude-opus-4-8`, else `OPENAI_API_KEY`
→ `gpt-5`, else `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) →
`gemini-2.5-pro`, else `ollama:llama3.1`. The initial Status event
records the chosen `provider:model` so you can verify.

### Model profiles

The base-URL env vars below bind one endpoint per wire protocol. To use
**several** endpoints of the same protocol in one session — e.g. first-party
OpenAI plus two OpenAI-compatible vendors — declare named profiles in
`config.toml` and switch between them at runtime with `/model @<name>`.

Each `[smith.models.<name>]` entry sets:

- `provider` — wire protocol to speak: `openai`, `anthropic`, `gemini`,
  `grok`, or `ollama`. (OAuth providers can't be profiled — use their prefixes
  directly.)
- `base_url` — endpoint URL (defaults to the protocol's public endpoint).
- `api_key_env` — name of the env var holding the key (preferred). Or
  `api_key = "..."` inline (discouraged). If neither is set, the protocol's
  standard key env var is used (`OPENAI_API_KEY`, etc.).
- `model` — default model name; override per call with `@<name>:<model>`.

```toml
[smith.models.deepseek]
provider    = "openai"
base_url    = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
model       = "deepseek-chat"

[smith.models.groq-llama]
provider    = "openai"
base_url    = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model       = "llama-3.3-70b-versatile"

[smith.models.xai]
provider    = "grok"
api_key_env = "XAI_API_KEY"
model       = "grok-2-latest"
```

```text
construct new smith "..." --model @deepseek   # start on a profile
/model openai:gpt-5                            # first-party OpenAI
/model @deepseek                               # DeepSeek
/model @groq-llama:llama-3.1-8b-instant        # Groq, one-off model override
/model                                         # shows current + lists @profiles
```

Profiles are always referenced with the explicit `@` prefix; bare names never
resolve to a profile. The status line shows `@<name>:<model>` so you can tell
which endpoint is active.

### Tools

Local: `shell`, `read_file`, `write_file`, `edit_file` (search/replace
with required uniqueness), `list_dir`, `find_files`.

Agentd-control (16 tools, same surface as `construct-mcp`):
`agentd_list_sessions`, `agentd_create_session`, `agentd_send_input`,
`agentd_get_output`, `agentd_get_diff`, `agentd_pin_session`,
`agentd_rename_session`, … — full read + write access to other
sessions on the same daemon. `agentd_whoami` returns the session id
this smith is running inside (auto-injected via env).

Browser: `browser_open`, `browser_inspect`, `browser_screenshot`, and
`browser_eval` drive Chrome through DevTools and emit the same browser
preview thumbnail that the TUI renders above the session. These tools
are native to smith and are also exposed through `construct-mcp` for
MCP-capable harnesses.

### Approval / automode

Tool calls run with your permissions, so smith classifies each tool
as **Safe** (read-only — `read_file`, `list_dir`, `find_files`, all
`agentd_get_*`/`agentd_list_*`) or **Risky** (mutates fs/sessions —
everything else, including `shell`).

- **automode off (default)**: Safe runs silently; Risky pauses with a
  minibuffer prompt showing the tool + arg summary + risk badge.
- **automode on**: all tools run silently. Modeline shows
  `[automode]`.

Approval prompt keys: `y`/Enter approve, `n`/Esc deny, `a` approve **and
flip automode on for this session**. Toggle automode anytime with
`C-x A` (emacs) / `A` (vim). Denied calls return a synthetic "user
denied" result to the model so it can pivot rather than crash.

Override the initial state with `CONSTRUCT_SMITH_AUTOMODE=1` (useful for
scripted/batch runs).

### Long output handling

The full tool output goes to the transcript (you see everything). The
agent's context only gets a truncated head + `[N bytes elided]` + tail
(8 KiB budget per call), so a `find /` doesn't blow the context
window.

Context budget is also pruned automatically: estimated tokens past 70%
of the model's window drops the oldest turn pair, always keeping the
two most-recent.

### Opt-out / customization

- `CONSTRUCT_SMITH_AUTOMODE=1` — start with automode on.
- `CONSTRUCT_SMITH_MODEL=<spec>` — default model when `--model` is
  omitted.
- `CONSTRUCT_CLAUDE_BIN` / `CONSTRUCT_CLAUDE_CMD` — choose the `claude` CLI
  used by `claude-oauth:`.
- `GEMINI_API_KEY` / `GOOGLE_API_KEY` — Gemini credentials (either is
  accepted).
- `GROK_API_KEY` / `XAI_API_KEY` — xAI Grok API credentials (either is
  accepted).
- `GROK_HOME` — override the base directory used by `grok-oauth:` token lookup;
  Smith reads `$GROK_HOME/.grok/auth.json` instead of `~/.grok/auth.json`.
- `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` / `GEMINI_BASE_URL` /
  `OLLAMA_HOST` — point at alternate endpoints. Pointing `OPENAI_BASE_URL`
  at an OpenAI-compatible vendor (OpenRouter, DeepSeek, Groq, xAI,
  Mistral, …) reuses the `openai:` path with no extra config. These bind
  one endpoint per protocol; to switch between several at runtime, use
  [Model profiles](#model-profiles) instead.
