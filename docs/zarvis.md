# zarvis built-in agent

`zarvis` is the built-in agent that ships with agentd. It talks to OpenAI,
Anthropic, or a local Ollama directly and runs its own agent loop with shell +
filesystem + agentd-control tools. No external CLI install required. Many PRs
for the agentd repository have already been made from Zarvis sessions running
inside agentd.

### Quick start

```sh
# Pick a provider — only one of these needs to be set:
export ANTHROPIC_API_KEY=sk-ant-...
# or  export OPENAI_API_KEY=sk-...
# or  run a local ollama (default http://localhost:11434)

agent new zarvis "list the rust files in this repo and summarize what each crate does"
```

### Model selection

Pass `--model <spec>` on `agent new` (or set `AGENTD_ZARVIS_MODEL`).
The spec is one of:

- `openai:<name>` — e.g. `openai:gpt-5-mini`
- `anthropic:<name>` — e.g. `anthropic:claude-haiku-4-5`
- `ollama:<name>` — e.g. `ollama:llama3.1`

Bare names auto-detect: `gpt-*` / `o[1-5]*` → OpenAI, `claude-*` →
Anthropic, anything else → Ollama. When in doubt, use the explicit
prefix.

If you don't pass a model and `AGENTD_ZARVIS_MODEL` isn't set, zarvis
picks: `ANTHROPIC_API_KEY` → `claude-haiku-4-5`, else `OPENAI_API_KEY`
→ `gpt-5-mini`, else `ollama:llama3.1`. The initial Status event
records the chosen `provider:model` so you can verify.

### Tools

Local: `shell`, `read_file`, `write_file`, `edit_file` (search/replace
with required uniqueness), `list_dir`, `find_files`.

Agentd-control (16 tools, same surface as `agentd-mcp`):
`agentd_list_sessions`, `agentd_create_session`, `agentd_send_input`,
`agentd_get_output`, `agentd_get_diff`, `agentd_pin_session`,
`agentd_rename_session`, … — full read + write access to other
sessions on the same daemon. `agentd_whoami` returns the session id
this zarvis is running inside (auto-injected via env).

Browser: `browser_open`, `browser_inspect`, `browser_screenshot`, and
`browser_eval` drive Chrome through DevTools and emit the same browser
preview thumbnail that the TUI renders above the session. These tools
are native to zarvis and are also exposed through `agentd-mcp` for
MCP-capable harnesses.

### Approval / automode

Tool calls run with your permissions, so zarvis classifies each tool
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

Override the initial state with `AGENTD_ZARVIS_AUTOMODE=1` (useful for
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

- `AGENTD_ZARVIS_AUTOMODE=1` — start with automode on.
- `AGENTD_ZARVIS_MODEL=<spec>` — default model when `--model` is
  omitted.
- `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` / `OLLAMA_HOST` — point at
  alternate endpoints (OpenAI-compatible vendors, self-hosted, etc.).
