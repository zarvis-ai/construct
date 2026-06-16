# smith built-in agent

`smith` is the built-in agent that ships with construct. It talks to OpenAI,
Anthropic, Google Gemini, or a local Ollama directly, and can also delegate
subscription auth to Codex or Claude Code. Smith runs its own agent loop with
shell + filesystem + construct-control tools. Many PRs for the construct
repository have already been made from smith sessions running inside construct.

### Quick start

```sh
# Pick a provider — only one of these needs to be set:
export ANTHROPIC_API_KEY=sk-ant-...
# or  export OPENAI_API_KEY=sk-...
# or  export GEMINI_API_KEY=...        # (or GOOGLE_API_KEY)
# or  codex login, then use --model codex-oauth:gpt-5
# or  claude login, then use --model claude-oauth:sonnet
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
- `ollama:<name>` — e.g. `ollama:llama3.1`
- `codex-oauth:<name>` — e.g. `codex-oauth:gpt-5-codex`

Bare names auto-detect: `gpt-*` / `o[1-5]*` → OpenAI, `claude-*` →
Anthropic, `gemini-*` → Gemini, anything else → Ollama. When in doubt,
use the explicit prefix.

`claude-oauth:` delegates model access to the installed `claude` CLI, so run
`claude login` first and keep `claude` on `PATH` (or set `CONSTRUCT_CLAUDE_BIN`
/ `CONSTRUCT_CLAUDE_CMD`). Smith disables Claude Code's built-in tools on this
path and asks Claude for structured Smith tool calls, so construct's normal
tool approvals and transcript persistence still apply. The child CLI process
does not inherit `ANTHROPIC_API_KEY` or third-party Claude provider env vars on
this path, so the explicit `claude-oauth:` prefix does not silently become API
key billing.

If you don't pass a model and `CONSTRUCT_SMITH_MODEL` isn't set, smith
picks: `ANTHROPIC_API_KEY` → `claude-opus-4-8`, else `OPENAI_API_KEY`
→ `gpt-5`, else `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) →
`gemini-2.5-pro`, else `ollama:llama3.1`. The initial Status event
records the chosen `provider:model` so you can verify.

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
- `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` / `GEMINI_BASE_URL` /
  `OLLAMA_HOST` — point at alternate endpoints. Pointing `OPENAI_BASE_URL`
  at an OpenAI-compatible vendor (OpenRouter, DeepSeek, Groq, xAI,
  Mistral, …) reuses the `openai:` path with no extra config.
